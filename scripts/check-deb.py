#!/usr/bin/env python3
"""Check a built Debian package (manta-linux-x86_64.deb or manta-linux-arm64.deb) the way an operator would install it (MAN-540).

Run as root on the machine under test, with the package's own `manta` and the
host's `apt-get`, `dpkg-query` and (with --systemd) `systemctl` on PATH. The
release workflow runs it twice per tag (docs/DECISIONS/2026-11-09-man540-debian-package.md):

Default (container) mode, in a clean `debian:bookworm` container with no
systemd:

- `apt-get install -y --no-install-recommends DEB` succeeds and dpkg reports
  the package `install ok installed`;
- `/usr/bin/manta` (0755), the unit `/usr/lib/systemd/system/manta.service`
  (0644, ExecStart runs /usr/bin/manta), the scaffold
  `/usr/share/manta/manta.toml` (0644) and `/etc/manta/manta.toml` (0600,
  owned by root, byte for byte `manta config init --out -`) are in place;
- `manta --version` runs and `manta config check` accepts the new config;
- the multi-user.target.wants link enables the unit, and dpkg lists no
  conffiles for the package;
- after an operator edit, `apt-get install -y --reinstall DEB` keeps the edit
  and leaves no `manta.toml.dpkg-*` file;
- `apt-get purge -y manta` removes /etc/manta, /usr/bin/manta and the link.

With --systemd, on the GitHub-hosted Ubuntu runner whose PID 1 is systemd:
the same install and layout checks, then `systemctl is-enabled` is `enabled`
and `is-active` is `inactive` (enabled, not started); `systemd-analyze verify`
accepts the unit; it writes SMOKE_CONFIG (a looping IQ recording from
`manta gen`) to /etc/manta/manta.toml, starts the service, waits for
`manta health` to exit 0, reads back the DynamicUser sandbox in HARDENING with
`systemctl show` (packaging/README.md "What the service may do" documents the
same table), checks the process runs as `manta` with NoNewPrivs and that
/var/cache/manta is systemd's `private/manta` link, reinstalls (the running
service must restart on a new MainPID and the config must not change), then
stops, purges and checks the unit is gone.

Release tooling only: it is not shipped in any archive or package. --root
points every file check at another tree (the tests' fake install); commands
always come from PATH. Tested by scripts/tests/test_package_deb.py.

Exit codes: 0 when every check passes, 1 when a check fails, 2 on a usage error.
"""
import argparse
import glob
import hashlib
import os
import shutil
import stat
import subprocess
import sys
import time

PACKAGE = "manta"
BINARY = "/usr/bin/manta"
UNIT = "/usr/lib/systemd/system/manta.service"
SCAFFOLD = "/usr/share/manta/manta.toml"
CONF_DIR = "/etc/manta"
CONF = "/etc/manta/manta.toml"
ENABLE_LINK = "/etc/systemd/system/multi-user.target.wants/manta.service"
# deb-systemd-helper looks for the unit under /lib/systemd/system before /usr/lib/systemd/system
# (init-system-helpers 1.65 and 1.66), so on a merged-/usr host, where /lib is a link to usr/lib,
# the enable link names the /lib path. Both name the packaged unit.
ENABLE_TARGETS = (UNIT, "/lib/systemd/system/manta.service")
CACHE_LINK = "/var/cache/manta"
CACHE_TARGET = "private/manta"
SYSTEMD_RUN_DIR = "/run/systemd/system"
MARKER = "# check-deb: operator edit"
SMOKE_DIR = "/var/lib/manta-check"
HEALTH_ADDR = "127.0.0.1:7302"
APT_TIMEOUT = 900
CMD_TIMEOUT = 60
POLL_SECS = 1.0

# The sandbox `DynamicUser=yes` implies, as `systemctl show` reports it. systemd 257 and later
# report PrivateTmp=disconnected for a DynamicUser service. packaging/README.md documents this
# table; scripts/tests/test_packaging.py keeps the two in step.
HARDENING = {
    "DynamicUser": {"yes"},
    "User": {"manta"},
    "NoNewPrivileges": {"yes"},
    "RestrictSUIDSGID": {"yes"},
    "ProtectSystem": {"strict"},
    "ProtectHome": {"read-only"},
    "PrivateTmp": {"yes", "disconnected"},
    "RemoveIPC": {"yes"},
}

# A hardware-free daemon: `manta gen --out /var/lib/manta-check v1` writes v1.wav, a 2-channel
# 96 kHz IQ recording, and `loop = true` keeps `manta run` up until it is stopped. Every listener
# keeps its default port; the metrics port (7302) answers `manta health`.
SMOKE_CONFIG = """\
[server]
station_callsign = "W3XYZ"
bind_addr = "127.0.0.1"

[input]
type = "file"
path = "/var/lib/manta-check/v1.wav"
iq = true
loop = true
"""


class Result:
    """One finished command: its exit code and captured output."""

    def __init__(self, returncode, stdout=b"", stderr=b""):
        self.returncode = returncode
        self.stdout_bytes = stdout
        self.stdout = stdout.decode("utf-8", "replace")
        self.stderr = stderr.decode("utf-8", "replace")

    def detail(self):
        text = (self.stderr.strip() or self.stdout.strip()).splitlines()
        return f"exit {self.returncode}" + (f": {text[-1]}" if text else "")


def run(argv, timeout=CMD_TIMEOUT, env=None):
    """Runs argv from PATH with stdin closed; a missing command is 127, a timeout 124."""
    try:
        p = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True,
                           timeout=timeout, env=env)
    except FileNotFoundError:
        return Result(127, b"", f"{argv[0]}: command not found".encode())
    except subprocess.TimeoutExpired:
        return Result(124, b"", f"{argv[0]}: timed out after {timeout} s".encode())
    return Result(p.returncode, p.stdout, p.stderr)


def apt_get(*args):
    env = dict(os.environ, DEBIAN_FRONTEND="noninteractive")
    return run(["apt-get", *args], timeout=APT_TIMEOUT, env=env)


class Checker:
    def __init__(self, root, expect_uid, timeout):
        self.root = root
        self.expect_uid = expect_uid
        self.timeout = timeout
        self.passed = []
        self.failed = []

    def at(self, path):
        return os.path.join(self.root, path.lstrip("/"))

    def check(self, name, ok, text):
        print(f"{'PASS' if ok else 'FAIL'}  {text}", flush=True)
        (self.passed if ok else self.failed).append(name)
        return ok

    def summary(self):
        if self.failed:
            print(f"check-deb: {len(self.failed)} failed: {', '.join(self.failed)} "
                  f"({len(self.passed)} passed)")
            return 1
        print(f"check-deb: {len(self.passed)} passed")
        return 0

    # Steps both modes share.

    def install(self, deb):
        r = apt_get("install", "-y", "--no-install-recommends", deb)
        if not self.check("install", r.returncode == 0,
                          f"apt-get install {deb}" + ("" if r.returncode == 0 else f": {r.detail()}")):
            return False
        q = run(["dpkg-query", "-W", "-f=${Status} ${Version} ${Architecture}", PACKAGE])
        fields = q.stdout.split()
        ok = q.returncode == 0 and q.stdout.startswith("install ok installed")
        shown = " ".join(fields[3:]) if ok else (q.stdout.strip() or q.detail())
        return self.check("installed", ok, f"installed {PACKAGE} {shown}" if ok
                          else f"dpkg-query reports {shown!r}, want 'install ok installed'")

    def mode_is(self, name, path, want):
        st = os.lstat(self.at(path)) if os.path.lexists(self.at(path)) else None
        ok = st is not None and stat.S_ISREG(st.st_mode) and stat.S_IMODE(st.st_mode) == want
        got = "missing" if st is None else (f"{stat.S_IMODE(st.st_mode):04o}"
                                            if stat.S_ISREG(st.st_mode) else "not a regular file")
        return self.check(name, ok, f"{path} is {want:04o}" if ok
                          else f"{path} is {got}, want a {want:04o} regular file")

    def layout(self):
        self.mode_is("binary", BINARY, 0o755)
        self.mode_is("unit", UNIT, 0o644)
        self.mode_is("scaffold", SCAFFOLD, 0o644)
        if self.mode_is("config-mode", CONF, 0o600):
            uid = os.stat(self.at(CONF)).st_uid
            self.check("config-owner", uid == self.expect_uid,
                       f"{CONF} is owned by uid {uid}" + ("" if uid == self.expect_uid
                                                          else f", want {self.expect_uid}"))
            init = run(["manta", "config", "init", "--out", "-"])
            with open(self.at(CONF), "rb") as f:
                same = init.returncode == 0 and f.read() == init.stdout_bytes
            self.check("config-content", same,
                       f"{CONF} matches `manta config init --out -`" if same
                       else f"{CONF} differs from `manta config init --out -` ({init.detail()})")
        self.unit_execstart()
        self.enable_link()
        conffiles = run(["dpkg-query", "-W", "-f=${Conffiles}", PACKAGE])
        ok = conffiles.returncode == 0 and not conffiles.stdout.strip()
        self.check("conffiles", ok, "dpkg lists no conffiles" if ok
                   else f"dpkg lists conffiles: {conffiles.stdout.strip() or conffiles.detail()}")
        version = run(["manta", "--version"])
        ok = version.returncode == 0 and version.stdout.startswith("manta ")
        self.check("version", ok, version.stdout.strip() if ok
                   else f"manta --version: {version.detail()}")

    def unit_execstart(self):
        try:
            with open(self.at(UNIT), encoding="utf-8") as f:
                lines = [ln for ln in f.read().splitlines() if ln.startswith("ExecStart=")]
        except OSError as e:
            return self.check("unit-execstart", False, f"{UNIT}: {e.strerror}")
        words = lines[0][len("ExecStart="):].split() if len(lines) == 1 else []
        ok = bool(words) and words[0] == BINARY
        return self.check("unit-execstart", ok, f"{UNIT} runs {BINARY}" if ok
                          else f"{UNIT} ExecStart lines {lines!r}, want one running {BINARY}")

    def enable_link(self):
        link = self.at(ENABLE_LINK)
        target = os.readlink(link) if os.path.islink(link) else None
        ok = target in ENABLE_TARGETS
        return self.check("enable-link", ok, f"{ENABLE_LINK} -> {target}" if ok
                          else f"{ENABLE_LINK} is {target or 'missing'}, want a link to {UNIT}")

    def leftover_dpkg_files(self):
        return sorted(os.path.basename(p) for p in glob.glob(self.at(CONF) + ".dpkg-*"))

    def purged(self):
        left = [p for p in (CONF_DIR, BINARY, ENABLE_LINK) if os.path.lexists(self.at(p))]
        return self.check("purge", not left, f"purge removed {CONF_DIR}, {BINARY} and the enable link"
                          if not left else f"purge left {', '.join(left)}")

    # Container mode.

    def container(self, deb):
        if not self.install(deb):
            return
        self.layout()
        r = run(["manta", "config", "check", "--config", CONF])
        self.check("config-check", r.returncode == 0, f"manta config check --config {CONF}"
                   + ("" if r.returncode == 0 else f": {r.detail()}"))
        with open(self.at(CONF), "a", encoding="utf-8") as f:
            f.write(MARKER + "\n")
        r = apt_get("install", "-y", "--reinstall", deb)
        if self.check("reinstall", r.returncode == 0, f"apt-get install --reinstall {deb}"
                      + ("" if r.returncode == 0 else f": {r.detail()}")):
            with open(self.at(CONF), encoding="utf-8") as f:
                kept = MARKER in f.read()
            left = self.leftover_dpkg_files()
            self.check("reinstall-config", kept and not left,
                       "reinstall kept the edited config" if kept and not left
                       else ("reinstall lost the operator edit" if not kept
                             else f"reinstall left {', '.join(left)}"))
        r = apt_get("purge", "-y", PACKAGE)
        if self.check("purge-run", r.returncode == 0, f"apt-get purge {PACKAGE}"
                      + ("" if r.returncode == 0 else f": {r.detail()}")):
            self.purged()

    # Systemd mode.

    def show(self, *props):
        r = run(["systemctl", "show", PACKAGE, *[a for p in props for a in ("-p", p)]])
        values = {}
        for line in r.stdout.splitlines():
            key, sep, value = line.partition("=")
            if sep:
                values[key] = value
        return values

    def main_pid(self):
        try:
            return int(self.show("MainPID").get("MainPID", "0"))
        except ValueError:
            return 0

    def wait_healthy(self):
        deadline = time.monotonic() + self.timeout
        while True:
            r = run(["manta", "health", "--addr", HEALTH_ADDR], timeout=10)
            if r.returncode == 0:
                return True
            if time.monotonic() >= deadline:
                journal = run(["journalctl", "-u", PACKAGE, "-n", "50", "--no-pager"])
                print(journal.stdout + journal.stderr, end="", flush=True)
                return False
            time.sleep(POLL_SECS)

    def wait_new_pid(self, old):
        deadline = time.monotonic() + self.timeout
        while True:
            pid = self.main_pid()
            if pid and pid != old:
                return pid
            if time.monotonic() >= deadline:
                return pid
            time.sleep(POLL_SECS)

    def config_digest(self):
        with open(self.at(CONF), "rb") as f:
            return hashlib.sha256(f.read()).hexdigest()

    def systemd(self, deb):
        if not self.install(deb):
            return
        enabled = run(["systemctl", "is-enabled", PACKAGE]).stdout.strip()
        active = run(["systemctl", "is-active", PACKAGE]).stdout.strip()
        self.check("enabled-not-started", enabled == "enabled" and active == "inactive",
                   f"enabled, not started (is-enabled: {enabled}, is-active: {active})")
        r = run(["systemd-analyze", "verify", UNIT])
        self.check("verify", r.returncode == 0, f"systemd-analyze verify {UNIT}"
                   + ("" if r.returncode == 0 else f": {r.detail()}"))
        self.layout()
        os.makedirs(self.at(SMOKE_DIR), mode=0o755, exist_ok=True)
        try:
            self.smoke(deb)
        finally:
            shutil.rmtree(self.at(SMOKE_DIR), ignore_errors=True)

    def smoke(self, deb):
        r = run(["manta", "gen", "--out", self.at(SMOKE_DIR), "v1"])
        if not self.check("smoke-recording", r.returncode == 0,
                          f"manta gen --out {SMOKE_DIR} v1" + ("" if r.returncode == 0 else f": {r.detail()}")):
            return
        fd = os.open(self.at(CONF), os.O_WRONLY | os.O_TRUNC | os.O_CREAT, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(SMOKE_CONFIG)
        os.chmod(self.at(CONF), 0o600)
        r = run(["manta", "config", "check", "--config", CONF])
        if not self.check("smoke-config", r.returncode == 0, "manta config check accepts the smoke config"
                          + ("" if r.returncode == 0 else f": {r.detail()}")):
            return
        r = run(["systemctl", "start", PACKAGE])
        healthy = r.returncode == 0 and self.wait_healthy()
        if not self.check("start", healthy, "started on a looping IQ recording; manta health: ok" if healthy
                          else f"systemctl start {PACKAGE}: {r.detail() if r.returncode else 'manta health never ok'}"):
            return
        self.sandbox()
        pid = self.main_pid()
        self.process(pid)
        digest = self.config_digest()
        r = apt_get("install", "-y", "--reinstall", deb)
        if self.check("reinstall", r.returncode == 0, f"apt-get install --reinstall {deb}"
                      + ("" if r.returncode == 0 else f": {r.detail()}")):
            new = self.wait_new_pid(pid)
            restarted = bool(new) and new != pid and self.wait_healthy()
            kept = self.config_digest() == digest
            self.check("reinstall-restart", restarted and kept,
                       f"reinstall kept the edited config and restarted the service (pid {pid} -> {new})"
                       if restarted and kept else
                       (f"reinstall changed {CONF}" if not kept
                        else f"service not restarted healthy after reinstall (pid {pid} -> {new})"))
        run(["systemctl", "stop", PACKAGE])
        r = apt_get("purge", "-y", PACKAGE)
        run(["systemctl", "daemon-reload"])
        still = run(["systemctl", "is-enabled", PACKAGE])
        ok = r.returncode == 0 and still.returncode != 0 and not os.path.lexists(self.at(CONF_DIR))
        self.check("purge", ok, f"purge removed {CONF_DIR} and the unit" if ok
                   else f"after purge: apt-get {r.detail()}, is-enabled exit {still.returncode} "
                        f"({still.stdout.strip()}), {CONF_DIR} "
                        f"{'present' if os.path.lexists(self.at(CONF_DIR)) else 'absent'}")

    def sandbox(self):
        values = self.show(*HARDENING, "NRestarts")
        wrong = [f"{k}={values.get(k, '')} (want {'|'.join(sorted(v))})"
                 for k, v in HARDENING.items() if values.get(k) not in v]
        self.check("sandbox", not wrong, "sandbox: " + " ".join(f"{k}={values.get(k)}" for k in HARDENING)
                   if not wrong else "sandbox: " + ", ".join(wrong))
        restarts = values.get("NRestarts")
        self.check("no-restarts", restarts == "0", f"NRestarts={restarts}")

    def process(self, pid):
        nnp = None
        try:
            with open(self.at(f"/proc/{pid}/status"), encoding="utf-8") as f:
                for line in f:
                    if line.startswith("NoNewPrivs:"):
                        nnp = line.split(":", 1)[1].strip()
        except OSError:
            pass
        user = run(["ps", "-o", "user=", "-p", str(pid)]).stdout.strip() if pid else ""
        ok = bool(pid) and nnp == "1" and user == PACKAGE
        cache = self.at(CACHE_LINK)
        target = os.readlink(cache) if os.path.islink(cache) else None
        self.check("process", ok, f"process {pid} runs as {user or '?'} with NoNewPrivs {nnp}"
                   + ("" if ok else f", want {PACKAGE} with NoNewPrivs 1"))
        self.check("cache-dir", target == CACHE_TARGET, f"{CACHE_LINK} -> {target}" if target == CACHE_TARGET
                   else f"{CACHE_LINK} is {target or 'not a link'}, want a link to {CACHE_TARGET}")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("deb", help="the .deb to install")
    ap.add_argument("--systemd", action="store_true",
                    help="also start the service under this host's systemd (needs /run/systemd/system)")
    ap.add_argument("--root", default="/", help="tree the file checks read (default /)")
    ap.add_argument("--expect-uid", type=int, default=0, help="owner of /etc/manta/manta.toml (default 0)")
    ap.add_argument("--timeout", type=float, default=90.0,
                    help="seconds to wait for the service to answer (default 90)")
    args = ap.parse_args(argv)
    deb = os.path.abspath(args.deb)
    if not os.path.isfile(deb):
        print(f"check-deb: no such package: {args.deb}", file=sys.stderr)
        return 2
    checker = Checker(args.root, args.expect_uid, args.timeout)
    if args.systemd:
        if not os.path.isdir(checker.at(SYSTEMD_RUN_DIR)):
            print(f"check-deb: --systemd needs a running systemd ({SYSTEMD_RUN_DIR} is missing)",
                  file=sys.stderr)
            return 2
        checker.systemd(deb)
    else:
        checker.container(deb)
    return checker.summary()


if __name__ == "__main__":
    sys.exit(main())
