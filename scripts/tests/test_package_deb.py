"""MAN-540: the Debian package (scripts/package-deb.py, packaging/deb/), its release-time checker
(scripts/check-deb.py) and the release workflow wiring.

Static tests run on every OS. Tests that build packages need `dpkg-deb`; tests that install them
into a scratch dpkg root need `dpkg` and `deb-systemd-helper` too (every Linux CI leg has them).
"""
import gzip
import hashlib
import importlib.util
import io
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / "scripts" / "package-deb.py"
CHECK_DEB = ROOT / "scripts" / "check-deb.py"
UNIT = ROOT / "packaging" / "systemd" / "manta.service"
SCAFFOLD = ROOT / "crates" / "manta-cli" / "src" / "config_init.toml"
DEB_DIR = ROOT / "packaging" / "deb"
SCRIPTS = ("postinst", "prerm", "postrm")
_spec = importlib.util.spec_from_file_location("_ci_workflow_text", os.path.join(HERE, "test_ci_trust_boundary.py"))
wf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(wf)
_hspec = importlib.util.spec_from_file_location("_package_deb", HELPER)
pdeb = importlib.util.module_from_spec(_hspec)
_hspec.loader.exec_module(pdeb)

KIT_EXEC_LINE = "ExecStart=/usr/local/bin/manta run --config ${CREDENTIALS_DIRECTORY}/manta.toml"
PKG_EXEC_LINE = "ExecStart=/usr/bin/manta run --config ${CREDENTIALS_DIRECTORY}/manta.toml"
MIGRATE_URL = ("https://github.com/HagaleTechnologies/manta/blob/main/packaging/README.md"
               "#moving-from-an-archive-install-to-the-package")
WROTE = "manta: wrote /etc/manta/manta.toml. manta.service is enabled but not started."
LIBASOUND2T64 = """Package: libasound2t64
Status: install ok installed
Maintainer: fake <fake@example.invalid>
Architecture: all
Version: 1.2.11-1ubuntu0.3
Provides: libasound2 (= 1.2.11-1ubuntu0.3)
Description: fake ALSA runtime
"""
LIBASOUND2 = LIBASOUND2T64.replace("libasound2t64", "libasound2").replace(
    "1.2.11-1ubuntu0.3", "1.2.8-1+b1").replace("Provides: libasound2 (= 1.2.8-1+b1)\n", "")
OLD_SYSTEMD = """Package: systemd
Status: install ok installed
Maintainer: fake <fake@example.invalid>
Architecture: all
Version: 245.4-4ubuntu3
Description: fake old systemd
"""
FAKE_PAGES = {"manta.1": b".TH manta 1\n", "manta-run.1": b".TH manta-run 1\n"}
EXPR = re.compile(r"\$\{\{\s*([^}]+?)\s*\}\}")
LINUX_ENTRIES = {"x86_64-unknown-linux-gnu": "manta-linux-x86_64", "aarch64-unknown-linux-gnu": "manta-linux-arm64"}

HAVE_DPKG_DEB = shutil.which("dpkg-deb") is not None
HAVE_SCRATCH = HAVE_DPKG_DEB and shutil.which("dpkg") is not None and shutil.which("deb-systemd-helper") is not None
HAVE_SH = shutil.which("sh") is not None and os.name == "posix"


def fake_elf(machine):
    """A 64-byte little-endian ELF64 header for e_machine 62 (x86_64) or 183 (aarch64)."""
    head = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9) + (2).to_bytes(2, "little") + machine.to_bytes(2, "little")
    return head + bytes(64 - len(head)) + b"fake manta\n"


ELF = {"x86_64-unknown-linux-gnu": fake_elf(62), "aarch64-unknown-linux-gnu": fake_elf(183)}


def write_file(path, data, mode=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    if mode is not None:
        os.chmod(path, mode)


def fake_man_dir(parent):
    man = os.path.join(parent, "man pages")
    for name, data in FAKE_PAGES.items():
        write_file(os.path.join(man, name), data)
    write_file(os.path.join(man, "notes.txt"), b"not a page\n")
    return man


def run_helper(*args, env=None):
    return subprocess.run([sys.executable, str(HELPER), *args], capture_output=True, text=True, env=env)


def build(tmp, target="x86_64-unknown-linux-gnu", version="0.1.0", root=ROOT, binary=None, env=None):
    """Build a package from a fake binary into a fresh dir under tmp; returns (deb path, binary bytes)."""
    out = tempfile.mkdtemp(dir=tmp, prefix="out-")
    data = binary if binary is not None else ELF[target]
    bin_path = os.path.join(out + "-in", "manta")
    write_file(bin_path, data, 0o755)
    man = fake_man_dir(out + "-in")
    proc = run_helper("--binary", bin_path, "--target", target, "--version", version, "--artifact", "pkg",
                      "--man-dir", man, "--out-dir", out, "--root", str(root), env=env)
    if proc.returncode != 0:
        raise AssertionError(f"package-deb failed: {proc.stderr}")
    return os.path.join(out, "pkg.deb"), data


def copy_root(dest):
    """The repository inputs package-deb.py reads, copied under dest."""
    for rel in ("packaging/systemd/manta.service", "crates/manta-cli/src/config_init.toml",
                "packaging/deb/postinst", "packaging/deb/prerm", "packaging/deb/postrm",
                "packaging/deb/copyright", "scripts/package-deb.py"):
        src = ROOT / rel
        write_file(os.path.join(dest, *rel.split("/")), src.read_bytes(), stat.S_IMODE(src.stat().st_mode))
    return dest


def scratch_root(tmp, extra_status=LIBASOUND2T64):
    root = tempfile.mkdtemp(dir=tmp, prefix="root-")
    for d in ("info", "updates", "triggers"):
        os.makedirs(os.path.join(root, "var", "lib", "dpkg", d))
    write_file(os.path.join(root, "var", "lib", "dpkg", "available"), b"")
    write_file(os.path.join(root, "var", "lib", "dpkg", "status"), extra_status.encode())
    return root


def dpkg_root(root, *args):
    return subprocess.run(["dpkg", f"--root={root}", "--force-not-root", "--force-script-chrootless",
                           "--no-triggers", "--log=/dev/null", *args],
                          stdin=subprocess.DEVNULL, capture_output=True, text=True)


def dpkg_status(root):
    return subprocess.run(["dpkg-query", f"--admindir={root}/var/lib/dpkg", "-W", "-f=${Status}", "manta"],
                          capture_output=True, text=True).stdout


def ar_members(path):
    with open(path, "rb") as f:
        data = f.read()
    assert data[:8] == b"!<arch>\n"
    names, i = [], 8
    while i < len(data):
        header = data[i:i + 60]
        names.append(header[:16].decode().strip().rstrip("/"))
        size = int(header[48:58].decode().strip())
        i += 60 + size + (size % 2)
    return names


def contents(deb):
    """{path: (type char, mode, owner)} from dpkg-deb --contents."""
    out = subprocess.run(["dpkg-deb", "--contents", deb], capture_output=True, text=True, check=True).stdout
    entries = {}
    for line in out.splitlines():
        parts = line.split()
        perms, owner, path = parts[0], parts[1], parts[-1]
        mode = 0
        for ch, bit in zip(perms[1:], (0o400, 0o200, 0o100, 0o40, 0o20, 0o10, 0o4, 0o2, 0o1)):
            if ch != "-":
                mode |= bit
        entries[path] = (perms[0], mode, owner)
    return entries


def script_text(name):
    return (DEB_DIR / name).read_text(encoding="utf-8")


def github_slug(heading):
    s = heading.strip().lower()
    s = re.sub(r"[^\w\- ]", "", s)
    return s.replace(" ", "-")


def build_steps():
    return wf.steps(wf.job_block(wf.read("release-publish.yml"), "build"))


def executable(body):
    return "\n".join(wf.code_lines(body or ""))


def step_named(steps, name):
    hits = [i for i, s in enumerate(steps) if wf.field(s, "name") == name]
    return hits


def matrix_entries():
    """[{key: value}] for build's matrix include list."""
    block = wf.job_block(wf.read("release-publish.yml"), "build")
    entries, cur = [], None
    for line in block:
        m = re.match(r"^\s*-\s+target:\s*(\S+)", line)
        if m:
            cur = {"target": m.group(1)}
            entries.append(cur)
            continue
        m = re.match(r"^\s+(os|artifact|use_cross|exe_ext):\s*(\S+)", line)
        if m and cur is not None:
            cur[m.group(1)] = m.group(2)
        if re.match(r"^\s+steps:", line):
            break
    return entries


class VersionAndTargetTests(unittest.TestCase):
    def test_release_versions_map_to_debian_versions(self):
        for given, want in (("0.1.0", "0.1.0-1"), ("0.2.0-rc.1", "0.2.0~rc.1-1"),
                            ("1.0.0-alpha-2", "1.0.0~alpha-2-1"), ("dispatch-123", "0~dispatch.123-1")):
            with self.subTest(given=given):
                self.assertEqual(pdeb.debian_version(given), want)

    def test_bad_versions_are_usage_errors(self):
        for bad in ("v0.1.0", "1.2", "dispatch-", "dispatch-x", "0.1.0+linux"):
            with self.subTest(version=bad), tempfile.TemporaryDirectory() as tmp:
                out = os.path.join(tmp, "out")
                bin_path = os.path.join(tmp, "manta")
                write_file(bin_path, ELF["x86_64-unknown-linux-gnu"])
                proc = run_helper("--binary", bin_path, "--target", "x86_64-unknown-linux-gnu", "--version", bad,
                                  "--artifact", "pkg", "--man-dir", fake_man_dir(tmp), "--out-dir", out)
                self.assertEqual(proc.returncode, 2, proc.stderr)
                self.assertFalse(os.path.exists(out) and os.listdir(out))

    @unittest.skipUnless(shutil.which("dpkg"), "needs dpkg")
    def test_debian_versions_sort_like_the_releases(self):
        order = [pdeb.debian_version(v) for v in ("dispatch-999", "0.1.0", "0.2.0-rc.1", "0.2.0-rc.2",
                                                  "0.2.0-rc.10", "0.2.0")]
        for lo, hi in zip(order, order[1:]):
            with self.subTest(lo=lo, hi=hi):
                self.assertEqual(subprocess.run(["dpkg", "--compare-versions", lo, "lt", hi]).returncode, 0)

    def test_targets_map_to_architectures(self):
        self.assertEqual(pdeb.architecture("x86_64-unknown-linux-gnu"), "amd64")
        self.assertEqual(pdeb.architecture("aarch64-unknown-linux-gnu"), "arm64")
        with tempfile.TemporaryDirectory() as tmp:
            bin_path = os.path.join(tmp, "manta")
            write_file(bin_path, ELF["x86_64-unknown-linux-gnu"])
            proc = run_helper("--binary", bin_path, "--target", "x86_64-apple-darwin", "--version", "0.1.0",
                              "--artifact", "pkg", "--man-dir", fake_man_dir(tmp), "--out-dir", tmp)
            self.assertEqual(proc.returncode, 2, proc.stderr)


    def test_semver_pattern_is_release_version_sh_s(self):
        text = (ROOT / "scripts" / "release-version.sh").read_text(encoding="utf-8")
        m = re.search(r"^SEMVER_RE='([^']+)'$", text, re.M)
        self.assertIsNotNone(m)
        self.assertEqual(pdeb.SEMVER_RE.pattern, m.group(1))


class StagingTests(unittest.TestCase):
    def test_packaged_unit_differs_from_the_kit_unit_only_in_execstart(self):
        kit = UNIT.read_text(encoding="utf-8").splitlines()
        packaged = pdeb.packaged_unit(UNIT.read_text(encoding="utf-8")).splitlines()
        self.assertEqual(len(kit), len(packaged))
        diff = [(a, b) for a, b in zip(kit, packaged) if a != b]
        self.assertEqual(diff, [(KIT_EXEC_LINE, PKG_EXEC_LINE)])

    def test_unit_rewrite_refuses_zero_or_two_execstart_lines(self):
        kit = UNIT.read_text(encoding="utf-8")
        for text in (kit.replace(KIT_EXEC_LINE, PKG_EXEC_LINE), kit + KIT_EXEC_LINE + "\n"):
            with self.assertRaisesRegex(Exception, "ExecStart"):
                pdeb.packaged_unit(text)

    def test_wrong_architecture_binary_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            man = fake_man_dir(tmp)
            for data, needle in ((ELF["x86_64-unknown-linux-gnu"], ("x86_64", "aarch64")),
                                 (b"#!/bin/sh\necho not elf\n", ("ELF",))):
                bin_path = os.path.join(tmp, "manta")
                write_file(bin_path, data)
                out = tempfile.mkdtemp(dir=tmp)
                proc = run_helper("--binary", bin_path, "--target", "aarch64-unknown-linux-gnu", "--version",
                                  "0.1.0", "--artifact", "pkg", "--man-dir", man, "--out-dir", out)
                self.assertEqual(proc.returncode, 1, proc.stderr)
                for n in needle:
                    self.assertIn(n, proc.stderr)
                self.assertEqual(os.listdir(out), [])

    def test_missing_inputs_fail_without_a_package(self):
        with tempfile.TemporaryDirectory() as tmp:
            good_bin = os.path.join(tmp, "manta")
            write_file(good_bin, ELF["x86_64-unknown-linux-gnu"])
            man = fake_man_dir(tmp)
            no_manta1 = os.path.join(tmp, "partial man")
            write_file(os.path.join(no_manta1, "manta-run.1"), b".TH x 1\n")
            bad_root = copy_root(os.path.join(tmp, "bad root"))
            os.remove(os.path.join(bad_root, "crates", "manta-cli", "src", "config_init.toml"))
            cases = {"binary": (os.path.join(tmp, "absent"), man, str(ROOT)),
                     "manta.1": (good_bin, no_manta1, str(ROOT)),
                     "scaffold": (good_bin, man, bad_root)}
            for label, (binary, man_dir, root) in cases.items():
                with self.subTest(missing=label):
                    out = tempfile.mkdtemp(dir=tmp)
                    proc = run_helper("--binary", binary, "--target", "x86_64-unknown-linux-gnu", "--version",
                                      "0.1.0", "--artifact", "pkg", "--man-dir", man_dir, "--out-dir", out,
                                      "--root", root)
                    self.assertEqual(proc.returncode, 1, proc.stderr)
                    self.assertIn("package-deb: missing required input:", proc.stderr)
                    self.assertEqual(os.listdir(out), [])


class MaintainerScriptTests(unittest.TestCase):
    def test_scripts_are_posix_sh_with_set_e(self):
        for name in SCRIPTS:
            with self.subTest(script=name):
                text = script_text(name)
                self.assertTrue(text.startswith("#!/bin/sh\n"))
                self.assertIn("\nset -e\n", text)
                self.assertTrue(os.access(DEB_DIR / name, os.X_OK) or os.name != "posix")
                if HAVE_SH:
                    self.assertEqual(subprocess.run(["sh", "-n", str(DEB_DIR / name)]).returncode, 0)

    def test_postinst_enables_and_try_restarts_but_never_starts(self):
        text = script_text("postinst")
        self.assertIn("deb-systemd-helper enable 'manta.service'", text)
        self.assertNotIn("deb-systemd-invoke start", text)
        m = re.search(r'if \[ -n "\$2" \]; then\n(.*?)\n\s*fi', text, re.S)
        self.assertIsNotNone(m)
        self.assertIn("deb-systemd-invoke try-restart 'manta.service'", m.group(1))
        self.assertEqual(text.count("deb-systemd-invoke"), 1)

    def test_every_path_the_scripts_write_is_under_dpkg_root(self):
        allowed = ("/usr/bin/deb-systemd-helper",)
        total_run = 0
        for name in SCRIPTS:
            text = script_text(name)
            total_run += text.count("/run/systemd/system")
            with self.subTest(script=name):
                self.assertEqual(text.count("/run/systemd/system"), 1)
                for lineno, line in enumerate(text.splitlines(), 1):
                    if line.lstrip().startswith(("#", "echo ")) or "grep -q '^ExecStart=/usr/local/bin/manta'" in line \
                            or line.startswith("migrate="):
                        continue
                    for m in re.finditer(r"(?<![\w}])(/(?:etc|usr)/[\w./-]*)", line):
                        if m.group(1) in allowed:
                            continue
                        prefix = line[:m.start()]
                        self.assertTrue(prefix.endswith("${DPKG_ROOT:-}"),
                                        f"{name}:{lineno} {m.group(1)} is not under ${{DPKG_ROOT:-}}: {line}")
        self.assertEqual(total_run, 3)

    def test_migration_url_names_a_packaging_readme_heading(self):
        m = re.search(r'migrate="([^"]+)"', script_text("postinst"))
        self.assertEqual(m.group(1), MIGRATE_URL)
        anchor = m.group(1).split("#", 1)[1]
        readme = (ROOT / "packaging" / "README.md").read_text(encoding="utf-8")
        slugs = {github_slug(h) for h in re.findall(r"^#+ (.+)$", readme, re.M)}
        self.assertIn(anchor, slugs)

    def test_copyright_carries_the_mit_text_and_the_apache_pointer(self):
        text = (DEB_DIR / "copyright").read_text(encoding="utf-8")
        self.assertTrue(text.startswith("Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/\n"))
        self.assertIn("\nLicense: MIT or Apache-2.0\n", text)
        self.assertIn("/usr/share/common-licenses/Apache-2.0", text)
        para = text.split("\nLicense: MIT\n", 1)[1].split("\n\n", 1)[0]
        mit = "\n".join("" if line == " ." else line[1:] for line in para.splitlines())
        lic = (ROOT / "LICENSE-MIT").read_text(encoding="utf-8")
        self.assertEqual(mit.strip(), lic[lic.index("Permission"):].strip())


@unittest.skipUnless(HAVE_DPKG_DEB, "needs dpkg-deb")
class PackageTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)

    def field(self, deb, name):
        return subprocess.run(["dpkg-deb", "-f", deb, name], capture_output=True, text=True, check=True).stdout.strip()

    def test_control_fields(self):
        for target, arch in (("x86_64-unknown-linux-gnu", "amd64"), ("aarch64-unknown-linux-gnu", "arm64")):
            with self.subTest(target=target):
                deb, data = build(self.tmp, target=target, version="0.2.0-rc.1")
                want = {"Package": "manta", "Version": "0.2.0~rc.1-1", "Architecture": arch,
                        "Maintainer": "Hagale Technologies, LLC <tony@hagale.net>",
                        "Depends": "libasound2 (>= 1.0.16) | libasound2t64", "Breaks": "systemd (<< 247)",
                        "Section": "hamradio", "Priority": "optional",
                        "Homepage": "https://github.com/HagaleTechnologies/manta"}
                for k, v in want.items():
                    self.assertEqual(self.field(deb, k), v, k)
                self.assertGreaterEqual(int(self.field(deb, "Installed-Size")), len(data) // 1024)
                self.assertIn("SoapySDR", self.field(deb, "Description"))

    def test_contents_owners_and_modes(self):
        deb, _ = build(self.tmp)
        entries = contents(deb)
        files = {"./usr/bin/manta": 0o755, "./usr/lib/systemd/system/manta.service": 0o644,
                 "./usr/share/manta/manta.toml": 0o644, "./usr/share/doc/manta/copyright": 0o644}
        for page in FAKE_PAGES:
            files[f"./usr/share/man/man1/{page}.gz"] = 0o644
        dirs = {"./", "./etc/", "./etc/manta/", "./usr/", "./usr/bin/", "./usr/lib/", "./usr/lib/systemd/",
                "./usr/lib/systemd/system/", "./usr/share/", "./usr/share/manta/", "./usr/share/doc/",
                "./usr/share/doc/manta/", "./usr/share/man/", "./usr/share/man/man1/"}
        self.assertEqual(set(entries), set(files) | dirs)
        for path, (kind, mode, owner) in entries.items():
            with self.subTest(path=path):
                self.assertEqual(owner, "root/root")
                if path in dirs:
                    self.assertEqual((kind, mode), ("d", 0o755))
                else:
                    self.assertEqual((kind, mode), ("-", files[path]))
                    self.assertFalse(path.startswith("./etc/"))

    def test_payload_bytes(self):
        deb, data = build(self.tmp)
        x = os.path.join(self.tmp, "x")
        subprocess.run(["dpkg-deb", "-x", deb, x], check=True)
        self.assertEqual(Path(x, "usr/bin/manta").read_bytes(), data)
        self.assertEqual(Path(x, "usr/share/manta/manta.toml").read_bytes(), SCAFFOLD.read_bytes())
        for page, body in FAKE_PAGES.items():
            self.assertEqual(gzip.decompress(Path(x, "usr/share/man/man1", page + ".gz").read_bytes()), body)
        self.assertEqual(Path(x, "usr/lib/systemd/system/manta.service").read_text(encoding="utf-8"),
                         pdeb.packaged_unit(UNIT.read_text(encoding="utf-8")))

    def test_control_archive_has_no_conffiles_and_true_md5sums(self):
        deb, _ = build(self.tmp)
        ctrl = subprocess.run(["dpkg-deb", "--ctrl-tarfile", deb], capture_output=True, check=True).stdout
        with tarfile.open(fileobj=io.BytesIO(ctrl)) as tar:
            members = {m.name: m for m in tar.getmembers() if m.isfile()}
            self.assertEqual(set(members), {"./control", "./md5sums", "./postinst", "./prerm", "./postrm"})
            for s in SCRIPTS:
                self.assertEqual(stat.S_IMODE(members["./" + s].mode), 0o755)
            md5sums = tar.extractfile(members["./md5sums"]).read().decode()
        x = os.path.join(self.tmp, "x")
        subprocess.run(["dpkg-deb", "-x", deb, x], check=True)
        lines = md5sums.splitlines()
        self.assertEqual(len(lines), 4 + len(FAKE_PAGES))
        for line in lines:
            digest, rel = line.split("  ", 1)
            self.assertEqual(hashlib.md5(Path(x, rel).read_bytes()).hexdigest(), digest, rel)

    def test_members_are_xz_compressed(self):
        deb, _ = build(self.tmp)
        self.assertEqual(ar_members(deb), ["debian-binary", "control.tar.xz", "data.tar.xz"])

    def test_build_replaces_no_partial_file(self):
        stub = os.path.join(self.tmp, "stub")
        write_file(os.path.join(stub, "dpkg-deb"), b"#!/bin/sh\necho stub failure >&2\nexit 1\n", 0o755)
        env = dict(os.environ, PATH=stub + os.pathsep + os.environ["PATH"])
        out = os.path.join(self.tmp, "out")
        os.mkdir(out)
        bin_path = os.path.join(self.tmp, "manta")
        write_file(bin_path, ELF["x86_64-unknown-linux-gnu"])
        proc = run_helper("--binary", bin_path, "--target", "x86_64-unknown-linux-gnu", "--version", "0.1.0",
                          "--artifact", "pkg", "--man-dir", fake_man_dir(self.tmp), "--out-dir", out, env=env)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("dpkg-deb failed", proc.stderr)
        self.assertEqual(os.listdir(out), [])


@unittest.skipUnless(HAVE_SCRATCH, "needs dpkg, dpkg-deb and deb-systemd-helper")
class ScratchInstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)

    def assertInstalled(self, proc, root):
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(dpkg_status(root), "install ok installed")

    def test_fresh_install_creates_the_config_and_enables_without_starting(self):
        deb, _ = build(self.tmp)
        root = scratch_root(self.tmp)
        proc = dpkg_root(root, "-i", deb)
        self.assertInstalled(proc, root)
        conf = Path(root, "etc/manta/manta.toml")
        self.assertEqual(stat.S_IMODE(conf.stat().st_mode), 0o600)
        self.assertEqual(conf.read_bytes(), SCAFFOLD.read_bytes())
        link = os.path.join(root, "etc/systemd/system/multi-user.target.wants/manta.service")
        self.assertEqual(os.readlink(link), "/usr/lib/systemd/system/manta.service")
        self.assertIn(WROTE, proc.stdout)

    def test_upgrade_keeps_an_edited_config_without_a_prompt_when_the_scaffold_changed(self):
        old, _ = build(self.tmp, version="0.1.0")
        new_root = copy_root(os.path.join(self.tmp, "next"))
        scaffold = Path(new_root, "crates/manta-cli/src/config_init.toml")
        scaffold.write_bytes(scaffold.read_bytes() + b"# a setting the next release documents\n")
        new, _ = build(self.tmp, version="0.1.1", root=new_root)
        root = scratch_root(self.tmp)
        self.assertInstalled(dpkg_root(root, "-i", old), root)
        conf = Path(root, "etc/manta/manta.toml")
        edited = conf.read_bytes() + b'\n[server]\nstation_callsign = "K1ABC"\n'
        conf.write_bytes(edited)
        proc = dpkg_root(root, "-i", new)
        self.assertInstalled(proc, root)
        self.assertEqual(conf.read_bytes(), edited)
        self.assertEqual(Path(root, "usr/share/manta/manta.toml").read_bytes(), scaffold.read_bytes())
        self.assertEqual([n for n in os.listdir(os.path.join(root, "etc/manta")) if ".dpkg-" in n], [])
        self.assertNotIn("wrote /etc/manta/manta.toml", proc.stdout)

    def test_archive_install_config_is_kept_and_the_shadows_are_named(self):
        deb, _ = build(self.tmp)
        root = scratch_root(self.tmp)
        custom = b'[server]\nstation_callsign = "W1AW"\n'
        write_file(os.path.join(root, "etc/manta/manta.toml"), custom, 0o600)
        write_file(os.path.join(root, "etc/systemd/system/manta.service"), UNIT.read_bytes(), 0o644)
        write_file(os.path.join(root, "usr/local/bin/manta"), b"old\n", 0o755)
        proc = dpkg_root(root, "-i", deb)
        self.assertInstalled(proc, root)
        self.assertEqual(Path(root, "etc/manta/manta.toml").read_bytes(), custom)
        for needle in ("/etc/systemd/system/manta.service", "/usr/local/bin/manta", MIGRATE_URL):
            self.assertIn(needle, proc.stderr)
        self.assertNotIn(WROTE, proc.stdout)

    def test_remove_keeps_the_config_and_purge_deletes_it(self):
        deb, _ = build(self.tmp)
        root = scratch_root(self.tmp)
        self.assertInstalled(dpkg_root(root, "-i", deb), root)
        link = os.path.join(root, "etc/systemd/system/multi-user.target.wants/manta.service")
        proc = dpkg_root(root, "-r", "manta")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue(os.path.isfile(os.path.join(root, "etc/manta/manta.toml")))
        self.assertTrue(os.path.islink(link))
        self.assertFalse(os.path.exists(os.path.join(root, "usr/bin/manta")))
        proc = dpkg_root(root, "-P", "manta")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(root, "etc/manta")))
        self.assertFalse(os.path.lexists(link))

    def test_old_systemd_is_refused(self):
        deb, _ = build(self.tmp)
        root = scratch_root(self.tmp, LIBASOUND2T64 + "\n" + OLD_SYSTEMD)
        proc = dpkg_root(root, "-i", deb)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("manta breaks systemd (<< 247)", proc.stderr)

    def test_alsa_is_required_and_either_package_satisfies_it(self):
        deb, _ = build(self.tmp)
        proc = dpkg_root(scratch_root(self.tmp, ""), "-i", deb)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("manta depends on libasound2 (>= 1.0.16) | libasound2t64", proc.stderr)
        for label, status in (("bookworm libasound2", LIBASOUND2), ("libasound2t64", LIBASOUND2T64)):
            with self.subTest(status=label):
                root = scratch_root(self.tmp, status)
                self.assertInstalled(dpkg_root(root, "-i", deb), root)


@unittest.skipUnless(HAVE_SH, "needs sh")
class SystemdRunningScriptTests(unittest.TestCase):
    """The maintainer scripts as a systemd host runs them, with deb-systemd-* and systemctl stubbed."""

    def setUp(self):
        self.tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.tmp)
        self.prefix = os.path.join(self.tmp, "host")
        write_file(os.path.join(self.prefix, "usr/share/manta/manta.toml"), SCAFFOLD.read_bytes())
        os.makedirs(os.path.join(self.prefix, "etc/manta"))
        run_dir = os.path.join(self.tmp, "run-systemd-system")
        os.makedirs(run_dir)
        self.bin = os.path.join(self.tmp, "bin")
        self.log = os.path.join(self.tmp, "calls.log")
        stub = ('#!/bin/sh\necho "$(basename "$0") $*" >> "$STUB_LOG"\n'
                'if grep -qxF "$(basename "$0") $*" "$STUB_FAILING"; then exit 3; fi\nexit 0\n')
        for cmd in ("deb-systemd-helper", "deb-systemd-invoke", "systemctl"):
            write_file(os.path.join(self.bin, cmd), stub.encode(), 0o755)
        self.scripts = {}
        for name in SCRIPTS:
            text = script_text(name).replace("/run/systemd/system", run_dir)
            text = text.replace("${DPKG_ROOT:-}/", self.prefix + "/")
            # The postrm's own existence test for the real helper points at the stub.
            text = text.replace('"/usr/bin/deb-systemd-helper"', f'"{self.bin}/deb-systemd-helper"')
            path = os.path.join(self.tmp, name)
            write_file(path, text.encode(), 0o755)
            self.scripts[name] = path

    def run_script(self, name, *args, failing=()):
        """Run a script; each stub call named in `failing` exits 3, every other one exits 0."""
        env = {k: v for k, v in os.environ.items() if k != "DPKG_ROOT"}
        failing_list = os.path.join(self.tmp, "failing")
        write_file(failing_list, "".join(call + "\n" for call in failing).encode())
        env.update(PATH=self.bin + os.pathsep + env.get("PATH", ""), STUB_LOG=self.log, STUB_FAILING=failing_list)
        if os.path.exists(self.log):
            os.remove(self.log)
        proc = subprocess.run(["sh", self.scripts[name], *args], env=env, capture_output=True, text=True,
                              stdin=subprocess.DEVNULL)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.stderr = proc.stderr
        return Path(self.log).read_text().splitlines() if os.path.exists(self.log) else []

    def test_first_configure_enables_and_never_starts(self):
        calls = self.run_script("postinst", "configure")
        self.assertEqual(calls, ["deb-systemd-helper unmask manta.service",
                                 "deb-systemd-helper --quiet was-enabled manta.service",
                                 "deb-systemd-helper enable manta.service",
                                 "systemctl --system daemon-reload"])

    def test_upgrade_configure_try_restarts(self):
        first = self.run_script("postinst", "configure")
        calls = self.run_script("postinst", "configure", "0.1.0-1")
        self.assertEqual(calls, first + ["deb-systemd-invoke try-restart manta.service"])

    def test_prerm_stops_only_on_remove(self):
        self.assertEqual(self.run_script("prerm", "upgrade", "0.1.1-1"), [])
        self.assertEqual(self.run_script("prerm", "remove"), ["deb-systemd-invoke stop manta.service"])

    def test_postrm_purge_deletes_the_config_and_purges_enable_state(self):
        self.run_script("postinst", "configure")
        conf = os.path.join(self.prefix, "etc/manta/manta.toml")
        self.assertTrue(os.path.isfile(conf))
        calls = self.run_script("postrm", "purge")
        self.assertEqual(calls, ["deb-systemd-helper purge manta.service"])
        self.assertFalse(os.path.exists(os.path.join(self.prefix, "etc/manta")))

    def test_a_failing_systemd_call_is_reported_and_never_fails_the_script(self):
        """Where debhelper's snippets ignore a call's exit status, the scripts print what failed and carry on."""
        unmask, was_enabled, enable, update_state = (f"deb-systemd-helper {a} manta.service" for a in
                                                     ("unmask", "--quiet was-enabled", "enable", "update-state"))
        reload, purge = "systemctl --system daemon-reload", "deb-systemd-helper purge manta.service"
        cases = (("postinst", ("configure", "0.1.0-1"),
                  (unmask, was_enabled, update_state, reload, "deb-systemd-invoke try-restart manta.service"),
                  ("unmasking manta.service", "recording manta.service's enable state",
                   "systemctl daemon-reload", "restarting manta.service")),
                 ("postinst", ("configure",), (enable,), ("enabling manta.service",)),
                 ("prerm", ("remove",), ("deb-systemd-invoke stop manta.service",), ("stopping manta.service",)),
                 ("postrm", ("remove",), (reload,), ("systemctl daemon-reload",)),
                 ("postrm", ("purge",), (purge,), ("purging manta.service's enable state",)))
        for name, args, failing, failures in cases:
            with self.subTest(script=name, args=args, failing=failing):
                calls = self.run_script(name, *args, failing=failing)
                self.assertTrue(set(failing) <= set(calls), calls)
                reported = [line for line in self.stderr.splitlines() if line.startswith("manta: ")]
                self.assertEqual(reported, [f"manta: {what} failed (exit 3); continuing." for what in failures])

    def test_purge_keeps_a_non_empty_etc_manta_and_never_fails_on_rmdir(self):
        conf_dir = os.path.join(self.prefix, "etc/manta")
        purge = ["deb-systemd-helper purge manta.service"]
        write_file(os.path.join(conf_dir, "operator-notes.txt"), b"mine\n")
        self.assertEqual(self.run_script("postrm", "purge"), purge)
        self.assertEqual(os.listdir(conf_dir), ["operator-notes.txt"])
        self.assertEqual(self.stderr, "")
        shutil.rmtree(conf_dir)
        self.assertEqual(self.run_script("postrm", "purge"), purge, "a missing /etc/manta skips only the rmdir")
        # A link to a directory passes the emptiness test, and rmdir refuses it: reported, not fatal.
        os.makedirs(os.path.join(self.tmp, "elsewhere"))
        os.symlink(os.path.join(self.tmp, "elsewhere"), conf_dir)
        self.assertEqual(self.run_script("postrm", "purge"), purge)
        self.assertTrue(os.path.islink(conf_dir))
        reported = [line for line in self.stderr.splitlines() if line.startswith("manta: ")]
        self.assertEqual(len(reported), 1, self.stderr)
        self.assertRegex(reported[0], r"^manta: removing /etc/manta failed \(exit \d+\); continuing\.$")


class ReleaseWorkflowTests(unittest.TestCase):
    def test_linux_legs_package_a_deb_after_the_archive_and_before_the_upload(self):
        steps = build_steps()
        hits = [i for i, s in enumerate(steps) if "scripts/package-deb.py" in executable(wf.run_body(s))]
        self.assertEqual(len(hits), 1)
        step = steps[hits[0]]
        self.assertEqual(wf.field(step, "name"), "Package (.deb)")
        self.assertEqual(wf.field(step, "if"), "runner.os == 'Linux'")
        self.assertEqual(wf.field(step, "shell"), "bash")
        self.assertEqual(wf.field(wf.sub(step, "env"), "VERSION"), "${{ needs.validate-tag.outputs.version }}")
        body = executable(wf.run_body(step))
        for needle in ('--binary "target/${{ matrix.target }}/release/${BIN_NAME}"', '--target "${{ matrix.target }}"',
                       '--version "$VERSION"', '--artifact "${{ matrix.artifact }}"', "--man-dir man-pages",
                       "--out-dir ."):
            self.assertIn(needle, body)
        archive = [i for i, s in enumerate(steps) if "scripts/package-release.py" in executable(wf.run_body(s))]
        upload = [i for i, s in enumerate(steps) if (wf.field(s, "uses") or "").startswith("actions/upload-artifact@")]
        self.assertLess(archive[0], hits[0])
        self.assertLess(hits[0], upload[0])

    def test_every_linux_leg_is_a_cross_leg(self):
        linux = {e["target"]: e for e in matrix_entries() if e.get("os", "").startswith("ubuntu-")}
        self.assertEqual(set(linux), set(LINUX_ENTRIES))
        for target, entry in linux.items():
            self.assertEqual(entry.get("use_cross"), "true", target)
            self.assertEqual(entry["artifact"], LINUX_ENTRIES[target])

    @unittest.skipUnless(HAVE_DPKG_DEB and shutil.which("bash"), "needs dpkg-deb and bash")
    def test_deb_step_builds_each_linux_package(self):
        steps = build_steps()
        body = wf.run_body(steps[step_named(steps, "Package (.deb)")[0]])
        for target, artifact in LINUX_ENTRIES.items():
            with self.subTest(target=target), tempfile.TemporaryDirectory() as tmp:
                values = {"matrix.target": target, "matrix.artifact": artifact}
                unknown = {e for e in EXPR.findall(body) if e not in values}
                self.assertFalse(unknown)
                script = EXPR.sub(lambda m: values[m.group(1)], body)
                work = copy_root(os.path.join(tmp, "work tree"))
                write_file(os.path.join(work, "target", target, "release", "manta"), ELF[target], 0o755)
                for name, data in FAKE_PAGES.items():
                    write_file(os.path.join(work, "man-pages", name), data)
                env = dict(os.environ, VERSION="0.1.0", BIN_NAME="manta")
                proc = subprocess.run(["bash", "--noprofile", "--norc", "-c", script], cwd=work, env=env,
                                      capture_output=True, text=True)
                self.assertEqual(proc.returncode, 0, proc.stderr)
                deb = os.path.join(work, artifact + ".deb")
                arch = subprocess.run(["dpkg-deb", "-f", deb, "Architecture"], capture_output=True, text=True).stdout
                self.assertEqual(arch.strip(), pdeb.TARGET_ARCH[target])
                x = os.path.join(tmp, "x")
                subprocess.run(["dpkg-deb", "-x", deb, x], check=True)
                self.assertEqual(Path(x, "usr/bin/manta").read_bytes(), ELF[target])

    def test_bookworm_check_runs_the_package_in_a_clean_container(self):
        steps = build_steps()
        hits = step_named(steps, "Verify the .deb in a clean Debian bookworm")
        self.assertEqual(len(hits), 1)
        step = steps[hits[0]]
        self.assertEqual(wf.field(step, "if"), "runner.os == 'Linux'")
        body = executable(wf.run_body(step))
        self.assertIn("docker run --rm --platform", body)
        mounts = re.findall(r'-v "([^"]+)"', body)
        self.assertEqual(mounts, ["$PWD/${{ matrix.artifact }}.deb:/release/${{ matrix.artifact }}.deb:ro",
                                  "$PWD/scripts/check-deb.py:/release/check-deb.py:ro"])
        self.assertIn("debian:bookworm", body)
        self.assertLess(body.index("apt-get update"), body.index("apt-get install -y --no-install-recommends python3\n"))
        self.assertIn("python3 /release/check-deb.py /release/${{ matrix.artifact }}.deb", body)
        self.assertNotRegex(body, r"(^|\s)(-e|--env)(\s|=)")
        self.assertNotIn("GITHUB_TOKEN", body)
        for target, platform in (("aarch64-unknown-linux-gnu", "linux/arm64"), ("x86_64-unknown-linux-gnu", "linux/amd64")):
            script = body.replace("${{ matrix.target }}", target).replace("${{ matrix.artifact }}", "x")
            script = script.replace("docker run", "echo docker run").split("debian:bookworm")[0]
            if shutil.which("bash"):
                out = subprocess.run(["bash", "-c", script + " true"], capture_output=True, text=True).stdout
                self.assertIn(f"--platform {platform}", out, target)

    def test_qemu_is_set_up_only_for_the_arm64_check(self):
        steps = build_steps()
        qemu = [i for i, s in enumerate(steps) if (wf.field(s, "uses") or "").startswith("docker/setup-qemu-action@")]
        self.assertEqual(len(qemu), 1)
        self.assertEqual(wf.field(steps[qemu[0]], "if"), "matrix.target == 'aarch64-unknown-linux-gnu'")
        docker = wf.steps(wf.job_block(wf.read("release-publish.yml"), "docker-publish-release"))
        pins = {wf.field(s, "uses") for s in docker if (wf.field(s, "uses") or "").startswith("docker/setup-qemu-action@")}
        self.assertEqual(pins, {wf.field(steps[qemu[0]], "uses")})
        self.assertLess(qemu[0], step_named(steps, "Verify the .deb in a clean Debian bookworm")[0])

    def test_systemd_check_runs_on_the_x86_64_runner(self):
        steps = build_steps()
        hits = step_named(steps, "Verify the .deb under systemd on this runner")
        self.assertEqual(len(hits), 1)
        step = steps[hits[0]]
        self.assertEqual(wf.field(step, "if"), "matrix.target == 'x86_64-unknown-linux-gnu'")
        body = executable(wf.run_body(step))
        update = body.index("sudo apt-get update")
        check = body.index('sudo python3 scripts/check-deb.py --systemd "${{ matrix.artifact }}.deb"')
        self.assertLess(update, check)
        upload = [i for i, s in enumerate(steps) if (wf.field(s, "uses") or "").startswith("actions/upload-artifact@")]
        self.assertLess(step_named(steps, "Verify the .deb in a clean Debian bookworm")[0], hits[0])
        self.assertLess(hits[0], upload[0])

    def test_no_new_step_name_starts_with_manta(self):
        # Step names are `- name:` items; the upload/download artifact names docs_consistency.rs
        # reads sit under `with:` without the dash.
        text = wf.read("release-publish.yml")
        self.assertEqual(re.findall(r"^\s*- name:\s*['\"]?(manta-.*)$", text, re.M), [])
        step_names = [wf.field(s, "name") or "" for s in build_steps()]
        for new in ("Package (.deb)", "Set up QEMU for the arm64 package check",
                    "Verify the .deb in a clean Debian bookworm", "Verify the .deb under systemd on this runner"):
            self.assertIn(new, step_names)
            self.assertFalse(new.startswith("manta-"))

    def test_release_notes_point_debian_users_at_the_packages(self):
        text = wf.read("release-publish.yml")
        body = text[text.index("          body: |"):]
        for needle in ("manta-linux-x86_64.deb", "manta-linux-arm64.deb", "sudo apt install ./",
                       "/etc/manta/manta.toml", "systemctl start manta"):
            self.assertIn(needle, body)


class CiRegistrationTests(unittest.TestCase):
    COMMAND = "python3 -m unittest scripts.tests.test_package_deb -v"

    def test_module_runs_in_test_and_both_support_jobs(self):
        text = wf.read("ci-full.yml")
        for job in ("test", "ubuntu-support", "macos-support"):
            with self.subTest(job=job):
                hits = [s for s in wf.steps(wf.job_block(text, job)) if (wf.run_body(s) or "").strip() == self.COMMAND]
                self.assertEqual(len(hits), 1)
                self.assertEqual(wf.field(hits[0], "if"), "runner.os != 'Windows'")

    def test_claude_md_pre_push_list_names_this_module(self):
        text = (ROOT / "CLAUDE.md").read_text(encoding="utf-8")
        m = re.search(r"python3 -m unittest -v scripts\.tests\.test_\{([^}]*)\}", text)
        self.assertIsNotNone(m)
        self.assertIn("package_deb", re.split(r"[,\s\\]+", m.group(1)))


# MAN-540 Phase 2: scripts/check-deb.py, the release-time checker, against a fake installed tree
# and stub commands.

# The stub every faked command runs (`<bin>/<name>` execs it with its own path first). It logs
# each call, whether stdin was /dev/null and DEBIAN_FRONTEND, and answers from the JSON scenario
# CK_SCENARIO names; apt-get applies the install, reinstall and purge effects to the fake root.
_CK_STUB = r'''
import json, os, shutil, stat, sys
name = os.path.basename(sys.argv[1])
args = sys.argv[2:]
with open(os.environ["CK_SCENARIO"], encoding="utf-8") as f:
    sc = json.load(f)
def at(p):
    return os.path.join(sc["root"], p.lstrip("/"))
phase_file = os.path.join(sc["state"], "phase")
def phase():
    if not os.path.exists(phase_file):
        return "new"
    with open(phase_file) as f:
        return f.read()
def set_phase(s):
    with open(phase_file, "w") as f:
        f.write(s)
try:
    st = os.fstat(0)
    devnull = stat.S_ISCHR(st.st_mode) and st.st_rdev == os.stat(os.devnull).st_rdev
except OSError:
    devnull = True
with open(sc["log"], "a", encoding="utf-8") as f:
    f.write(json.dumps({"cmd": name, "args": args, "devnull": devnull,
                        "frontend": os.environ.get("DEBIAN_FRONTEND")}) + "\n")
def write(p, text, mode):
    path = at(p)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(text)
    os.chmod(path, mode)
UNIT = "/usr/lib/systemd/system/manta.service"
LINK = "/etc/systemd/system/multi-user.target.wants/manta.service"
if name == "apt-get":
    if args[0] == "install" and "--reinstall" not in args:
        write("/usr/bin/manta", "#!/bin/sh\n", 0o755)
        write(UNIT, "[Service]\nExecStart=" + sc["execstart"]
              + " run --config ${CREDENTIALS_DIRECTORY}/manta.toml\n", 0o644)
        write("/usr/share/manta/manta.toml", sc["scaffold"], 0o644)
        write("/etc/manta/manta.toml", sc["config"], sc["config_mode"])
        if sc["enable_target"]:
            os.makedirs(os.path.dirname(at(LINK)), exist_ok=True)
            os.symlink(sc["enable_target"], at(LINK))
        set_phase("installed")
        sys.exit(sc["install_rc"])
    if args[0] == "install":
        if sc["reinstall"] == "rewrite":
            write("/etc/manta/manta.toml", sc["scaffold"], 0o600)
        elif sc["reinstall"] == "dpkg-new":
            write("/etc/manta/manta.toml.dpkg-new", sc["scaffold"], 0o644)
        set_phase("reinstalled")
        sys.exit(0)
    if args[0] == "purge":
        for p in ("/etc/manta", "/usr/bin/manta", UNIT, LINK):
            if p in sc["purge_leaves"]:
                continue
            if os.path.isdir(at(p)) and not os.path.islink(at(p)):
                shutil.rmtree(at(p))
            elif os.path.lexists(at(p)):
                os.remove(at(p))
        set_phase("purged")
        sys.exit(0)
elif name == "dpkg-query":
    fmt = next(a for a in args if a.startswith("-f="))
    sys.stdout.write(sc["conffiles"] if "Conffiles" in fmt else "install ok installed 0.2.0-1 amd64")
elif name == "manta":
    if args == ["--version"]:
        print("manta 0.2.0 (git 0123456789ab; features: none)")
    elif args[:2] == ["config", "init"]:
        sys.stdout.write(sc["scaffold"])
    elif args[:2] == ["config", "check"]:
        sys.exit(sc["config_check_rc"])
    elif args[0] == "gen":
        out = args[args.index("--out") + 1]
        os.makedirs(out, exist_ok=True)
        open(os.path.join(out, args[-1] + ".wav"), "wb").close()
    elif args[0] == "health":
        sys.exit(sc["health_rc"])
elif name == "systemctl":
    if args[0] == "is-enabled":
        if phase() == "purged" and not sc["enabled_after_purge"]:
            print("Failed to get unit file state for manta.service: No such file or directory",
                  file=sys.stderr)
            sys.exit(1)
        print("enabled")
    elif args[0] == "is-active":
        print(sc["is_active"])
        sys.exit(0 if sc["is_active"] == "active" else 3)
    elif args[0] == "show":
        values = dict(sc["show"])
        values["MainPID"] = str(sc["pid_after"] if phase() == "reinstalled" else sc["pid_before"])
        for i, a in enumerate(args):
            if a == "-p":
                print(args[i + 1] + "=" + values.get(args[i + 1], ""))
elif name == "systemd-analyze":
    sys.exit(sc["verify_rc"])
elif name == "ps":
    print(sc["ps_user"])
elif name == "journalctl":
    print("-- fake journal --")
'''

_CK_SCAFFOLD = "# manta.toml: the fake scaffold `manta config init` writes\n#[server]\n"
_CK_SHOW = {"DynamicUser": "yes", "User": "manta", "NoNewPrivileges": "yes",
            "RestrictSUIDSGID": "yes", "ProtectSystem": "strict", "ProtectHome": "read-only",
            "PrivateTmp": "yes", "RemoveIPC": "yes", "NRestarts": "0"}
_CK_DEB = "manta-linux-x86_64.deb"


def _ck_load_check_deb():
    spec = importlib.util.spec_from_file_location("check_deb", CHECK_DEB)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _ck_run(tmp, *args, systemd_dir=False, nnp="1", cache="link", no_deb=False, **over):
    """Runs check-deb.py from `tmp` (the .deb given relative) against a fake root and stub
    commands; returns (process, the logged calls, the absolute .deb path)."""
    tmp = Path(os.path.realpath(tmp))
    root, state, bindir = tmp / "root", tmp / "state", tmp / "bin"
    for d in (root, state, bindir):
        d.mkdir(exist_ok=True)
    if systemd_dir:
        (root / "run" / "systemd" / "system").mkdir(parents=True)
    for pid in (4242, 4311):
        (root / "proc" / str(pid)).mkdir(parents=True)
        (root / "proc" / str(pid) / "status").write_text(f"Name:\tmanta\nNoNewPrivs:\t{nnp}\n")
    (root / "var" / "cache").mkdir(parents=True)
    if cache == "link":
        (root / "var" / "cache" / "manta").symlink_to("private/manta")
    else:
        (root / "var" / "cache" / "manta").mkdir()
    stub = tmp / "stub.py"
    stub.write_text(_CK_STUB)
    for name in ("apt-get", "dpkg-query", "manta", "systemctl", "systemd-analyze", "ps", "journalctl"):
        path = bindir / name
        path.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{stub}" "$0" "$@"\n')
        path.chmod(0o755)
    scenario = dict(root=str(root), state=str(state), log=str(tmp / "log.jsonl"),
                    execstart="/usr/bin/manta", scaffold=_CK_SCAFFOLD, config=_CK_SCAFFOLD,
                    config_mode=0o600, enable_target="/usr/lib/systemd/system/manta.service",
                    install_rc=0, reinstall="keep", purge_leaves=[], conffiles="",
                    config_check_rc=0, health_rc=0, enabled_after_purge=False,
                    is_active="inactive", show=dict(_CK_SHOW), pid_before=4242, pid_after=4311,
                    verify_rc=0, ps_user="manta")
    scenario.update(over)
    (tmp / "scenario.json").write_text(json.dumps(scenario))
    if not no_deb:
        (tmp / _CK_DEB).write_bytes(b"!<arch>\n")
    env = dict(os.environ, PATH=f"{bindir}{os.pathsep}{os.environ.get('PATH', '')}",
               CK_SCENARIO=str(tmp / "scenario.json"))
    env.pop("DEBIAN_FRONTEND", None)
    argv = [sys.executable, str(CHECK_DEB), "--root", str(root), "--expect-uid", str(os.getuid()),
            "--timeout", "1", *args]
    p = subprocess.run(argv, cwd=tmp, env=env, capture_output=True, text=True, timeout=120,
                       stdin=subprocess.DEVNULL)
    log = tmp / "log.jsonl"
    calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
    return p, calls, str(tmp / _CK_DEB)


def _ck_apt_calls(calls):
    return [c["args"] for c in calls if c["cmd"] == "apt-get"]


@unittest.skipUnless(os.name == "posix", "stub commands are POSIX shell scripts")
class CheckerContainerTests(unittest.TestCase):
    """scripts/check-deb.py in a clean container: install, layout, reinstall, purge (MAN-540)."""

    def test_a_correct_install_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            p, calls, deb = _ck_run(tmp, _CK_DEB)
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertNotIn("FAIL", p.stdout)
        self.assertRegex(p.stdout.strip().splitlines()[-1], r"^check-deb: \d+ passed$")
        self.assertIn("PASS  installed manta 0.2.0-1 amd64", p.stdout)
        self.assertTrue(os.path.isabs(deb))
        self.assertEqual(_ck_apt_calls(calls), [
            ["install", "-y", "--no-install-recommends", deb],
            ["install", "-y", "--reinstall", deb],
            ["purge", "-y", "manta"],
        ])
        for call in calls:
            self.assertTrue(call["devnull"], call)
            if call["cmd"] == "apt-get":
                self.assertEqual(call["frontend"], "noninteractive", call)

    def test_a_merged_usr_enable_link_through_lib_passes(self):
        # deb-systemd-helper checks /lib/systemd/system first; on bookworm and noble /lib is usr/lib.
        with tempfile.TemporaryDirectory() as tmp:
            proc, _, _ = _ck_run(tmp, _CK_DEB, enable_target="/lib/systemd/system/manta.service")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("multi-user.target.wants/manta.service -> /lib/systemd/system/manta.service", proc.stdout)

    def test_each_broken_install_fails_naming_what_is_wrong(self):
        cases = [
            ("config mode 0644", (), dict(config_mode=0o644), "/etc/manta/manta.toml is 0644"),
            ("config owner", ("--expect-uid", str(os.getuid() + 1)), {}, "owned by uid"),
            ("config differs from init", (), dict(config=_CK_SCAFFOLD + "# edited\n"),
             "differs from `manta config init --out -`"),
            ("unit runs another binary", (), dict(execstart="/usr/local/bin/manta"), "ExecStart"),
            ("enable link missing", (), dict(enable_target=None),
             "multi-user.target.wants/manta.service is missing"),
            ("enable link elsewhere", (), dict(enable_target="/etc/systemd/system/manta.service"),
             "is /etc/systemd/system/manta.service, want a link"),
            ("conffiles listed", (), dict(conffiles=" /etc/manta/manta.toml 0123abcd"),
             "dpkg lists conffiles"),
            ("config check fails", (), dict(config_check_rc=1), "FAIL  manta config check"),
            ("reinstall loses the edit", (), dict(reinstall="rewrite"), "lost the operator edit"),
            ("reinstall leaves dpkg-new", (), dict(reinstall="dpkg-new"), "manta.toml.dpkg-new"),
            ("purge leaves /etc/manta", (), dict(purge_leaves=["/etc/manta"]), "purge left /etc/manta"),
        ]
        for label, extra, over, needle in cases:
            with self.subTest(label), tempfile.TemporaryDirectory() as tmp:
                p, _, _ = _ck_run(tmp, *extra, _CK_DEB, **over)
                self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
                fails = [ln for ln in p.stdout.splitlines() if ln.startswith("FAIL")]
                self.assertTrue(any(needle in ln for ln in fails), p.stdout)
                self.assertRegex(p.stdout.strip().splitlines()[-1],
                                 r"^check-deb: \d+ failed: .+ \(\d+ passed\)$")

    def test_usage_errors_exit_2(self):
        with tempfile.TemporaryDirectory() as tmp:
            p, calls, _ = _ck_run(tmp, no_deb=True)
            self.assertEqual(p.returncode, 2, p.stdout + p.stderr)
        with tempfile.TemporaryDirectory() as tmp:
            p, calls, _ = _ck_run(tmp, "missing.deb", no_deb=True)
            self.assertEqual(p.returncode, 2, p.stdout + p.stderr)
            self.assertIn("no such package", p.stderr)
            self.assertEqual(calls, [])
        with tempfile.TemporaryDirectory() as tmp:
            p, calls, _ = _ck_run(tmp, "--systemd", _CK_DEB)
            self.assertEqual(p.returncode, 2, p.stdout + p.stderr)
            self.assertIn("/run/systemd/system", p.stderr)
            self.assertEqual(calls, [])


@unittest.skipUnless(os.name == "posix", "stub commands are POSIX shell scripts")
class CheckerSystemdTests(unittest.TestCase):
    """scripts/check-deb.py --systemd on a host whose PID 1 is systemd (MAN-540)."""

    def run_systemd(self, tmp, **kw):
        return _ck_run(tmp, "--systemd", _CK_DEB, systemd_dir=True, **kw)

    def test_a_correct_service_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            p, calls, deb = self.run_systemd(tmp)
            leftover = os.path.exists(os.path.join(tmp, "root", "var", "lib", "manta-check"))
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertNotIn("FAIL", p.stdout)
        self.assertRegex(p.stdout.strip().splitlines()[-1], r"^check-deb: \d+ passed$")
        for needle in ("enabled, not started (is-enabled: enabled, is-active: inactive)",
                       "sandbox: DynamicUser=yes User=manta NoNewPrivileges=yes RestrictSUIDSGID=yes "
                       "ProtectSystem=strict ProtectHome=read-only PrivateTmp=yes RemoveIPC=yes",
                       "process 4242 runs as manta with NoNewPrivs 1",
                       "/var/cache/manta -> private/manta",
                       "(pid 4242 -> 4311)"):
            self.assertIn(needle, p.stdout)
        self.assertFalse(leftover, "the smoke recording directory must be removed")
        self.assertEqual(_ck_apt_calls(calls), [
            ["install", "-y", "--no-install-recommends", deb],
            ["install", "-y", "--reinstall", deb],
            ["purge", "-y", "manta"],
        ])
        verbs = [c["args"][0] for c in calls if c["cmd"] == "systemctl"]
        self.assertLess(verbs.index("start"), verbs.index("stop"))
        self.assertIn("daemon-reload", verbs)
        self.assertTrue(all(c["devnull"] for c in calls))

    def test_each_hardening_property_is_checked(self):
        module = _ck_load_check_deb()
        for key in module.HARDENING:
            with self.subTest(key), tempfile.TemporaryDirectory() as tmp:
                p, _, _ = self.run_systemd(tmp, show=dict(_CK_SHOW, **{key: "wrong"}))
                self.assertEqual(p.returncode, 1, p.stdout)
                self.assertTrue(any(ln.startswith("FAIL  sandbox:") and f"{key}=wrong" in ln
                                    for ln in p.stdout.splitlines()), p.stdout)
        with tempfile.TemporaryDirectory() as tmp:
            p, _, _ = self.run_systemd(tmp, show=dict(_CK_SHOW, PrivateTmp="disconnected"))
            self.assertEqual(p.returncode, 0, p.stdout)

    def test_each_broken_service_fails(self):
        cases = [
            ("active right after install", dict(is_active="active"), "is-active: active"),
            ("NoNewPrivs 0", dict(nnp="0"), "NoNewPrivs 0"),
            ("process user", dict(ps_user="root"), "runs as root"),
            ("cache is a plain directory", dict(cache="dir"), "/var/cache/manta is not a link"),
            ("never healthy", dict(health_rc=1), "manta health never ok"),
            ("not restarted by reinstall", dict(pid_after=4242), "not restarted"),
            ("reinstall changes the config", dict(reinstall="rewrite"),
             "reinstall changed /etc/manta/manta.toml"),
            ("still enabled after purge", dict(enabled_after_purge=True), "after purge"),
            ("restarted while running", dict(show=dict(_CK_SHOW, NRestarts="3")), "NRestarts=3"),
            ("unit fails verify", dict(verify_rc=1), "systemd-analyze verify"),
        ]
        for label, over, needle in cases:
            with self.subTest(label), tempfile.TemporaryDirectory() as tmp:
                p, _, _ = self.run_systemd(tmp, **over)
                self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
                self.assertTrue(any(ln.startswith("FAIL") and needle in ln
                                    for ln in p.stdout.splitlines()), p.stdout)

    def test_never_healthy_prints_the_journal(self):
        with tempfile.TemporaryDirectory() as tmp:
            p, calls, _ = self.run_systemd(tmp, health_rc=1)
        self.assertIn("-- fake journal --", p.stdout)
        self.assertIn(["-u", "manta", "-n", "50", "--no-pager"],
                      [c["args"] for c in calls if c["cmd"] == "journalctl"])

    def test_smoke_config_is_valid_for_manta(self):
        try:
            import tomllib
        except ImportError:
            self.skipTest("tomllib needs Python 3.11")
        module = _ck_load_check_deb()
        config = tomllib.loads(module.SMOKE_CONFIG)
        self.assertEqual(config["server"]["bind_addr"], "127.0.0.1")
        self.assertTrue(config["server"]["station_callsign"])
        self.assertEqual(config["input"]["type"], "file")
        self.assertIs(config["input"]["iq"], True)
        self.assertIs(config["input"]["loop"], True)
        path = config["input"]["path"]
        self.assertTrue(path.startswith("/var/lib/manta-check/") and os.path.isabs(path), path)
        self.assertEqual(os.path.dirname(path), module.SMOKE_DIR)


if __name__ == "__main__":
    unittest.main()
