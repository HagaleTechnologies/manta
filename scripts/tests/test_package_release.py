"""Tests for scripts/package-release.py and the release workflows' Package steps (MAN-268).

stdlib unittest only. Builds real tar.gz and zip archives from the repository's files and a fake
binary, then executes the workflows' own Package run: bodies against the helper.
Run: python3 -m unittest scripts.tests.test_package_release -v
"""
import fnmatch
import importlib.util
import os
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
HELPER = os.path.join(ROOT, "scripts", "package-release.py")
_spec = importlib.util.spec_from_file_location("_ci_workflow_text", os.path.join(HERE, "test_ci_trust_boundary.py"))
wf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(wf)

# Independent of the helper's own list on purpose: dropping a file from either the helper or the
# repository must fail a test here.
KIT = (
    "README.md",
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "manta.example.toml",
    "docker-compose.yml",
    "packaging/README.md",
    "packaging/systemd/manta.service",
    "packaging/launchd/com.hagaletechnologies.manta.plist",
    "packaging/launchd/com.hagaletechnologies.manta-logrotate.plist",
    "packaging/launchd/create-service-account.sh",
    "packaging/launchd/rotate-log.sh",
    "docs/RUNBOOKS/network-exposure.md",
    "docs/GUIDES/rbn-node.md",
    "docs/GUIDES/kiwisdr.md",
    "docs/GUIDES/raspberry-pi.md",
    "docs/RUNBOOKS/node-health.md",
    "docs/RUNBOOKS/uplink-health.md",
    "docs/RUNBOOKS/release.md",
    "docs/RUNBOOKS/secondary-skimmer-field-node.md",
    "docs/RUNBOOKS/m2-pi4-cpu-budget.md",
    "docs/DECISIONS/2026-07-25-m2-kiwisdr-input-pins.md",
    "docs/DECISIONS/2026-09-06-broad-review-decisions.md",
    "docs/SPEC-decode-core.md",
    "rust-toolchain.toml",
)
EXECUTABLE_KIT = {"packaging/launchd/create-service-account.sh", "packaging/launchd/rotate-log.sh"}
FAKE_BINARY = b"\x7fELF fake manta\x00\x01\x02"
RELEASE_WORKFLOWS = ("release.yml", "release-publish.yml")
OLD_PACKAGING = ("tar -C dist", "Compress-Archive")

# What `manta man-pages --out` produces, faked: two distinct pages (the set
# and per-page bytes are both asserted) plus a stray file that must NOT ship.
FAKE_MAN_PAGES = {
    "manta.1": b".TH manta 1 \"\" \"manta 0.0.0\" \"User Commands\"\n",
    "manta-run.1": b".TH manta-run 1 \"\" \"manta 0.0.0\" \"User Commands\"\n",
}


def fake_man_dir(parent, name="man pages"):
    """A directory holding the fake pages and a stray file; returns its path."""
    man_dir = os.path.join(parent, name)  # a space in the default, like every other path
    for name, data in FAKE_MAN_PAGES.items():
        write_file(os.path.join(man_dir, name), data)
    write_file(os.path.join(man_dir, "notes.txt"), b"not a man page\n")
    return man_dir


def repo_bytes(rel):
    with open(os.path.join(ROOT, *rel.split("/")), "rb") as f:
        return f.read()


def write_file(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def copy_kit(dest_root, skip=None):
    for rel in KIT:
        if rel != skip:
            write_file(os.path.join(dest_root, *rel.split("/")), repo_bytes(rel))


def run_helper(*args, root=None):
    cmd = [sys.executable, HELPER, *args] + (["--root", root] if root else [])
    return subprocess.run(cmd, capture_output=True, text=True)


def read_archive(path):
    """{name: (kind, mode, bytes)} for every member; kind is 'dir' or 'file'."""
    out = {}
    if path.endswith(".tar.gz"):
        with tarfile.open(path, "r:gz") as tar:
            for m in tar.getmembers():
                if m.isdir():
                    out[m.name.rstrip("/")] = ("dir", stat.S_IMODE(m.mode), None)
                elif m.isfile():
                    out[m.name] = ("file", stat.S_IMODE(m.mode), tar.extractfile(m).read())
                else:
                    out[m.name] = ("other", m.mode, None)
    else:
        with zipfile.ZipFile(path) as zf:
            for info in zf.infolist():
                mode = info.external_attr >> 16
                if info.is_dir():
                    out[info.filename.rstrip("/")] = ("dir", stat.S_IMODE(mode), None)
                else:
                    kind = "file" if stat.S_ISREG(mode) else "other"
                    out[info.filename] = (kind, stat.S_IMODE(mode), zf.read(info))
    return out


def extract(path, dest):
    if path.endswith(".tar.gz"):
        with tarfile.open(path, "r:gz") as tar:
            if hasattr(tarfile, "data_filter"):
                tar.extractall(dest, filter="data")
            else:
                tar.extractall(dest)
    else:
        with zipfile.ZipFile(path) as zf:
            zf.extractall(dest)


def workflow_steps(name):
    """(job, step lines) for every step of every job in .github/workflows/<name>."""
    text = wf.read(name)
    return [(job, step) for job in wf.job_ids(text) for step in wf.steps(wf.job_block(text, job))]


def executable(body):
    """A run: body without its shell comment lines."""
    return "\n".join(wf.code_lines(body or ""))


class ArchiveAssertions(unittest.TestCase):
    def assert_release_archive(self, path, artifact, binary_name, binary_bytes,
                               man_pages=FAKE_MAN_PAGES):
        members = read_archive(path)
        self.assertEqual({n.split("/")[0] for n in members}, {artifact}, "exactly one top-level directory")
        self.assertEqual(members.get(artifact, ("missing",))[0], "dir")
        for name in members:
            self.assertFalse(name.startswith("/") or ".." in name.split("/"), name)
        files = {n[len(artifact) + 1:]: v for n, v in members.items() if v[0] != "dir"}
        page_rels = tuple(f"man/man1/{n}" for n in man_pages)
        self.assertEqual(sorted(files), sorted((binary_name,) + KIT + page_rels))
        self.assertEqual({v[0] for v in files.values()}, {"file"}, "only regular files")
        self.assertEqual(files[binary_name][2], binary_bytes)
        for rel in KIT:
            self.assertEqual(files[rel][2], repo_bytes(rel), rel)
        for rel in page_rels:
            self.assertEqual(files[rel][2], man_pages[os.path.basename(rel)], rel)
            self.assertEqual(files[rel][1], 0o644, f"{rel} must be readable, not executable")
        for rel in (binary_name,) + tuple(EXECUTABLE_KIT):
            self.assertEqual(files[rel][1] & 0o755, 0o755, f"{rel} lost its executable mode")
        with tempfile.TemporaryDirectory() as tmp:
            extract(path, tmp)
            for rel in (binary_name,) + KIT + page_rels:
                with open(os.path.join(tmp, artifact, *rel.split("/")), "rb") as f:
                    expected = binary_bytes if rel == binary_name else (
                        man_pages[os.path.basename(rel)] if rel in page_rels else repo_bytes(rel))
                    self.assertEqual(f.read(), expected, rel)
            if os.name == "posix" and path.endswith(".tar.gz"):
                for rel in (binary_name,) + tuple(EXECUTABLE_KIT):
                    self.assertTrue(os.access(os.path.join(tmp, artifact, *rel.split("/")), os.X_OK), rel)
            # Operators can read the guides from the extracted release, including
            # their linked config reference, service kit and operational runbooks.
            top = os.path.join(tmp, artifact)
            for rel in KIT:
                if not rel.startswith("docs/GUIDES/"):
                    continue
                guide = os.path.join(top, *rel.split("/"))
                with open(guide, encoding="utf-8") as f:
                    links = re.findall(r"\]\(([^)\s]+)\)", f.read())
                for link in links:
                    if re.match(r"[a-z]+:", link) or link.startswith("#"):
                        continue
                    target = os.path.normpath(os.path.join(os.path.dirname(guide), link.split("#")[0]))
                    self.assertTrue(target.startswith(top + os.sep), link)
                    self.assertTrue(os.path.isfile(target), f"{rel}: {link} does not ship")


class HelperArchiveTests(ArchiveAssertions):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = os.path.join(self._tmp.name, "release work")  # a space in every path
        self.out = os.path.join(self.tmp, "out dir")
        os.makedirs(self.out)
        self.man = fake_man_dir(self.tmp)

    def tearDown(self):
        self._tmp.cleanup()

    def fake_binary(self, name):
        path = os.path.join(self.tmp, "build output", name)
        write_file(path, FAKE_BINARY)
        return path

    def test_tar_gz_contains_the_binary_readme_licences_and_kit(self):
        artifact = "manta-linux-x86_64"
        proc = run_helper("--binary", self.fake_binary("manta"), "--artifact", artifact,
                          "--format", "tar.gz", "--out-dir", self.out, "--man-dir", self.man)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        path = os.path.join(self.out, artifact + ".tar.gz")
        self.assertIn(path, proc.stdout)
        self.assertEqual(os.listdir(self.out), [artifact + ".tar.gz"])
        if os.name == "posix":  # not mkstemp's 0600: the old `tar -czf` honoured the umask
            umask = os.umask(0)
            os.umask(umask)
            self.assertEqual(stat.S_IMODE(os.stat(path).st_mode), 0o666 & ~umask)
        self.assert_release_archive(path, artifact, "manta", FAKE_BINARY)

    def test_zip_contains_the_binary_readme_licences_and_kit(self):
        artifact = "manta-windows-x86_64"
        proc = run_helper("--binary", self.fake_binary("manta.exe"), "--artifact", artifact,
                          "--format", "zip", "--out-dir", self.out, "--man-dir", self.man)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        path = os.path.join(self.out, artifact + ".zip")
        self.assertEqual(os.listdir(self.out), [artifact + ".zip"])
        self.assert_release_archive(path, artifact, "manta.exe", FAKE_BINARY)

    def test_stale_files_in_the_output_directory_are_not_packaged(self):
        artifact = "manta-macos-arm64"
        for stale in (os.path.join(self.out, "dist", artifact, "stale.txt"),
                      os.path.join(self.out, artifact, "stale.txt"),
                      os.path.join(self.out, artifact, "packaging", "README.md")):
            write_file(stale, b"stale\n")
        write_file(os.path.join(self.out, artifact + ".tar.gz"), b"not an archive")
        for fmt, ext in (("tar.gz", ".tar.gz"), ("zip", ".zip")):
            with self.subTest(format=fmt):
                proc = run_helper("--binary", self.fake_binary("manta"), "--artifact", artifact,
                                  "--format", fmt, "--out-dir", self.out, "--man-dir", self.man)
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assert_release_archive(os.path.join(self.out, artifact + ext), artifact, "manta",
                                            FAKE_BINARY)

    def test_each_missing_asset_fails_without_writing_an_archive(self):
        binary = self.fake_binary("manta")
        for rel in KIT:
            for fmt in ("tar.gz", "zip"):
                with self.subTest(missing=rel, format=fmt):
                    with tempfile.TemporaryDirectory() as tmp:
                        root = os.path.join(tmp, "repo copy")
                        out = os.path.join(tmp, "out dir")
                        os.makedirs(out)
                        copy_kit(root, skip=rel)
                        proc = run_helper("--binary", binary, "--artifact", "manta-linux-arm64",
                                          "--format", fmt, "--out-dir", out,
                                          "--man-dir", self.man, root=root)
                        self.assertNotEqual(proc.returncode, 0, f"archive built without {rel}")
                        self.assertIn(rel.split("/")[-1], proc.stderr)
                        self.assertEqual(os.listdir(out), [], "no archive or partial file may be left")

    def test_missing_binary_fails_without_writing_an_archive(self):
        proc = run_helper("--binary", os.path.join(self.tmp, "no such dir", "manta"),
                          "--artifact", "manta-linux-x86_64", "--format", "tar.gz",
                          "--out-dir", self.out, "--man-dir", self.man)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("no such dir", proc.stderr)
        self.assertEqual(os.listdir(self.out), [])

    def test_complete_copy_of_the_kit_packages_from_root(self):
        # Guards the missing-asset test above: the same copied root succeeds when nothing is removed.
        root = os.path.join(self.tmp, "repo copy")
        copy_kit(root)
        proc = run_helper("--binary", self.fake_binary("manta"), "--artifact", "manta-linux-arm64",
                          "--format", "tar.gz", "--out-dir", self.out,
                          "--man-dir", self.man, root=root)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assert_release_archive(os.path.join(self.out, "manta-linux-arm64.tar.gz"), "manta-linux-arm64",
                                    "manta", FAKE_BINARY)

    def test_artifact_must_be_one_path_component(self):
        for artifact in ("", "..", "dist/manta-linux-x86_64", "..\\manta"):
            with self.subTest(artifact=artifact):
                proc = run_helper("--binary", self.fake_binary("manta"), "--artifact", artifact,
                                  "--format", "tar.gz", "--out-dir", self.out,
                                  "--man-dir", self.man)
                self.assertEqual(proc.returncode, 2, proc.stderr)
                self.assertEqual(os.listdir(self.out), [])

    def test_man_dir_is_required(self):
        proc = run_helper("--binary", self.fake_binary("manta"), "--artifact", "manta-linux-x86_64",
                          "--format", "tar.gz", "--out-dir", self.out)
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn("--man-dir", proc.stderr)
        self.assertEqual(os.listdir(self.out), [])

    def test_missing_man_pages_fail_without_writing_an_archive(self):
        binary = self.fake_binary("manta")
        empty = os.path.join(self.tmp, "empty man dir")
        os.makedirs(empty)
        only_run = os.path.join(self.tmp, "only manta-run")
        write_file(os.path.join(only_run, "manta-run.1"), FAKE_MAN_PAGES["manta-run.1"])
        cases = (os.path.join(self.tmp, "no such dir"), only_run, empty)
        for fmt in ("tar.gz", "zip"):
            for man_dir in cases:
                with self.subTest(format=fmt, man_dir=man_dir):
                    out = os.path.join(self.tmp, f"out {fmt}")
                    os.makedirs(out, exist_ok=True)
                    proc = run_helper("--binary", binary, "--artifact", "manta-linux-arm64",
                                      "--format", fmt, "--out-dir", out, "--man-dir", man_dir)
                    self.assertNotEqual(proc.returncode, 0,
                                        f"an archive was built from {man_dir}")
                    self.assertIn("manta.1", proc.stderr)
                    self.assertEqual(os.listdir(out), [],
                                     "no archive or partial file may be left")


def package_steps(name):
    """(job, step) whose executable run: body invokes the helper."""
    return [(job, step) for job, step in workflow_steps(name)
            if "scripts/package-release.py" in executable(wf.run_body(step))]


def matrix_include(text, job="build"):
    """[{key: value}] for each item of the job's strategy.matrix.include, comments dropped."""
    include = wf.sub(wf.sub(wf.sub(wf.job_block(text, job), "strategy"), "matrix"), "include")
    items = []
    for line in wf.code_lines(include[1:]):
        m = re.match(r"^\s*(- )?([A-Za-z_]+):\s*(.*)$", line)
        if not m:
            continue
        if m.group(1):
            items.append({})
        items[-1][m.group(2)] = m.group(3).strip().strip("'\"")
    return items


RUNNER_OS = {"ubuntu": "Linux", "macos": "macOS", "windows": "Windows"}
EXPR = re.compile(r"\$\{\{\s*([^}]*?)\s*\}\}")


def producer_package_step(name, job):
    """The one Package step of an archive-producing job."""
    found = [step for j, step in package_steps(name) if j == job]
    if len(found) != 1:
        raise AssertionError(f"{name} {job}: expected one step invoking scripts/package-release.py, "
                             f"found {len(found)}")
    return found[0]


# MAN-82: the build-soapy job's one archive, beside the build matrix's five.
SOAPY_JOB = "build-soapy"
SOAPY_ENTRY = {"target": "x86_64-unknown-linux-gnu", "os": "ubuntu-24.04",
               "artifact": "manta-linux-x86_64-soapy"}
SOAPY_MAN_DIR = "man-pages-soapy"
SOAPY_BINARY = "target/x86_64-unknown-linux-gnu/release/manta"
DEFAULT_ARTIFACTS = {"manta-macos-x86_64", "manta-macos-arm64", "manta-windows-x86_64",
                     "manta-linux-x86_64", "manta-linux-arm64"}


class WorkflowPackageStepTests(ArchiveAssertions):
    def test_each_release_workflow_packages_through_the_helper(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                found = package_steps(name)
                self.assertEqual(sorted(job for job, _ in found), ["build", SOAPY_JOB],
                                 f"{name}: expected one Package step in build and one in {SOAPY_JOB}")
                step = producer_package_step(name, "build")
                self.assertEqual(wf.field(step, "shell"), "bash")
                self.assertIsNone(wf.field(step, "if"), "the Package step must run on every OS")
                body = executable(wf.run_body(step))
                for arg in ("--binary ", '--artifact "${{ matrix.artifact }}"', "--format ",
                            "--man-dir man-pages", "--out-dir "):
                    self.assertIn(arg, body)
                self.assertIn("matrix.target", body)
                step = producer_package_step(name, SOAPY_JOB)
                self.assertEqual(wf.field(step, "shell"), "bash")
                self.assertIsNone(wf.field(step, "if"))
                body = executable(wf.run_body(step))
                for arg in (f'--binary "{SOAPY_BINARY.replace("/manta", "/${BIN_NAME}")}"',
                            f"--artifact {SOAPY_ENTRY['artifact']}", "--format tar.gz",
                            f"--man-dir {SOAPY_MAN_DIR}", "--out-dir ."):
                    self.assertIn(arg, body)
                self.assertNotIn("matrix.", body)

    def test_old_inline_packaging_is_gone(self):
        for name in RELEASE_WORKFLOWS:
            for job, step in workflow_steps(name):
                body = executable(wf.run_body(step))
                for old in OLD_PACKAGING:
                    with self.subTest(workflow=name, job=job, step=wf.field(step, "name"), old=old):
                        self.assertNotIn(old, body)

    def test_package_step_precedes_the_unchanged_upload(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                build = wf.steps(wf.job_block(wf.read(name), "build"))
                pkg = [i for i, s in enumerate(build)
                       if "scripts/package-release.py" in executable(wf.run_body(s))]
                upload = [i for i, s in enumerate(build)
                          if (wf.field(s, "uses") or "").startswith("actions/upload-artifact@")]
                self.assertEqual(len(pkg), 1)
                self.assertEqual(len(upload), 1)
                self.assertLess(pkg[0], upload[0])
                paths = wf.sub(wf.sub(build[upload[0]], "with"), "path")
                self.assertEqual([p.strip() for p in paths[1:] if p.strip()],
                                 ["${{ matrix.artifact }}.tar.gz", "${{ matrix.artifact }}.zip"])

    def test_soapy_package_step_precedes_its_upload(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                steps = wf.steps(wf.job_block(wf.read(name), SOAPY_JOB))
                pkg = [i for i, s in enumerate(steps)
                       if "scripts/package-release.py" in executable(wf.run_body(s))]
                upload = [i for i, s in enumerate(steps)
                          if (wf.field(s, "uses") or "").startswith("actions/upload-artifact@")]
                self.assertEqual(len(pkg), 1)
                self.assertEqual(len(upload), 1)
                self.assertLess(pkg[0], upload[0])
                with_block = wf.sub(steps[upload[0]], "with")
                self.assertEqual(wf.field(with_block, "name"), SOAPY_ENTRY["artifact"])
                self.assertEqual(wf.field(with_block, "path"), SOAPY_ENTRY["artifact"] + ".tar.gz")
                self.assertEqual(wf.field(with_block, "if-no-files-found"), "error")
                build_upload = [s for s in wf.steps(wf.job_block(wf.read(name), "build"))
                                if (wf.field(s, "uses") or "").startswith("actions/upload-artifact@")]
                self.assertEqual(wf.field(with_block, "retention-days"),
                                 wf.field(wf.sub(build_upload[0], "with"), "retention-days"))
                self.assertEqual(wf.field(steps[upload[0]], "uses"), wf.field(build_upload[0], "uses"))

    @unittest.skipIf(shutil.which("bash") is None, "bash is not installed")
    def test_package_step_builds_every_matrix_archive(self):
        bash = shutil.which("bash")
        for name in RELEASE_WORKFLOWS:
            text = wf.read(name)
            bin_name = re.search(r"^  BIN_NAME: (\S+)$", text, re.M).group(1)
            body = wf.run_body(producer_package_step(name, "build"))
            entries = matrix_include(text)
            self.assertIn(("x86_64-unknown-linux-gnu", ""),
                          {(e["target"], e.get("exe_ext", "")) for e in entries})
            self.assertIn(("x86_64-pc-windows-msvc", ".exe"),
                          {(e["target"], e.get("exe_ext", "")) for e in entries})
            built = set()
            for entry in entries:
                with self.subTest(workflow=name, target=entry["target"]):
                    built.add(self._run_step(bash, body, bin_name, entry))
            # MAN-82: build-soapy's own step, against only its own man pages.
            with self.subTest(workflow=name, job=SOAPY_JOB):
                built.add(self._run_step(bash, wf.run_body(producer_package_step(name, SOAPY_JOB)),
                                         bin_name, SOAPY_ENTRY, man_dir=SOAPY_MAN_DIR))
            self.assertEqual(built, {a + (".zip" if "windows" in a else ".tar.gz")
                                     for a in DEFAULT_ARTIFACTS | {SOAPY_ENTRY["artifact"]}},
                             f"{name}: six archives across the build matrix and {SOAPY_JOB}")

    def _run_step(self, bash, body, bin_name, entry, man_dir="man-pages"):
        runner_os = RUNNER_OS[entry["os"].split("-")[0]]
        values = {"runner.os": runner_os, "matrix.target": entry["target"],
                  "matrix.exe_ext": entry.get("exe_ext", ""), "matrix.artifact": entry["artifact"]}
        unknown = {e for e in EXPR.findall(body) if e not in values}
        self.assertFalse(unknown, f"Package step uses expressions this test cannot substitute: {unknown}")
        script = EXPR.sub(lambda m: values[m.group(1)], body)
        exe = "manta" + values["matrix.exe_ext"]
        windows = runner_os == "Windows"
        with tempfile.TemporaryDirectory() as tmp:
            work = os.path.join(tmp, "work tree")  # the checkout, with a space in its path
            copy_kit(work)
            write_file(os.path.join(work, "scripts", "package-release.py"), repo_bytes("scripts/package-release.py"))
            write_file(os.path.join(work, "target", entry["target"], "release", exe), FAKE_BINARY)
            write_file(os.path.join(work, "dist", entry["artifact"], "stale.txt"), b"old staging\n")
            # The download-artifact step materialises validate-tag's man-pages
            # artifact at work/man-pages/ before this step runs (build-soapy
            # renders its own into man-pages-soapy/); the name is the
            # workflow's, so no space here. Only that one directory exists.
            fake_man_dir(work, man_dir)
            log = os.path.join(tmp, "interpreters.log")
            # Functions shadow PATH lookup, so the run: body's own python/python3 choice is recorded
            # and still runs this interpreter on hosts that lack one of the two names.
            real = sys.executable.replace("\\", "/")
            shim = (f'python() {{ echo python >> "$SHIM_LOG"; "{real}" "$@"; }}\n'
                    f'python3() {{ echo python3 >> "$SHIM_LOG"; "{real}" "$@"; }}\n')
            path = os.path.join(tmp, "package.sh")
            with open(path, "wb") as f:  # LF endings even on Windows
                f.write((shim + script).encode("utf-8"))
            env = dict(os.environ, BIN_NAME=bin_name, SHIM_LOG=log.replace("\\", "/"))
            proc = subprocess.run([bash, "--noprofile", "--norc", "-eo", "pipefail", path],
                                  cwd=work, env=env, capture_output=True, text=True)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            with open(log, encoding="utf-8") as f:
                self.assertEqual(f.read().split(), ["python" if windows else "python3"])
            ext = ".zip" if windows else ".tar.gz"
            archives = sorted(n for n in os.listdir(work) if n.endswith((".zip", ".tar.gz")))
            self.assertEqual(archives, [entry["artifact"] + ext])
            self.assert_release_archive(os.path.join(work, entry["artifact"] + ext), entry["artifact"],
                                        exe, FAKE_BINARY)
            return entry["artifact"] + ext


class ManPagesJobTests(unittest.TestCase):
    """MAN-133: validate-tag renders every archive's man pages once, natively, after its tag
    checks; `build` needs only validate-tag (scripts/tests/release-version.test.sh)."""

    def features_of(self, body):
        m = re.search(r"--features (\S+)", body)
        self.assertIsNotNone(m, f"no --features in:\n{body}")
        return m.group(1)

    def test_man_pages_job_renders_once_natively(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                self.assertFalse(wf.job_block(wf.read(name), "man-pages"),
                                 f"{name}: a separate man-pages job would need `build` to need it too")
                job = wf.job_block(wf.read(name), "validate-tag")
                self.assertTrue(job, f"{name} has no validate-tag job")
                self.assertEqual(wf.field(job, "runs-on"), "ubuntu-latest")
                steps = wf.steps(job)
                self.assertTrue(steps, "validate-tag has no steps")
                checkout = [s for s in steps
                            if (wf.field(s, "uses") or "").startswith("actions/checkout@")]
                self.assertEqual(len(checkout), 1, "expected exactly one checkout step")
                bodies = [executable(wf.run_body(s)) for s in steps if wf.run_body(s)]
                build = [i for i, s in enumerate(steps)
                         if "cargo build --release -p manta-cli" in executable(wf.run_body(s))]
                self.assertEqual(len(build), 1, "expected exactly one host build step")
                self.assertRegex(executable(wf.run_body(steps[build[0]])), r"--features \S+")
                checks = [i for i, s in enumerate(steps)
                          if re.search(r"release-version\.sh|check-release-ancestry\.sh",
                                       executable(wf.run_body(s)))]
                self.assertTrue(checks, "validate-tag lost its tag checks")
                self.assertLess(max(checks), build[0],
                                "the man-page build must come after every tag check")
                self.assertTrue(
                    any("target/release/manta man-pages --out man-pages" in b for b in bodies),
                    "no step runs `target/release/manta man-pages --out man-pages`")
                uploads = [s for s in steps
                           if (wf.field(s, "uses") or "").startswith("actions/upload-artifact@")]
                matched = [s for s in uploads
                           if wf.field(wf.sub(s, "with"), "name") == "man-pages"]
                self.assertEqual(len(matched), 1, "expected exactly one man-pages upload")
                with_block = wf.sub(matched[0], "with")
                self.assertEqual(wf.field(with_block, "path"), "man-pages")
                self.assertEqual(wf.field(with_block, "if-no-files-found"), "error")

    def test_man_pages_use_the_build_legs_features(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                job = wf.job_block(wf.read(name), "validate-tag")
                man_bodies = [executable(wf.run_body(s)) for s in wf.steps(job) if wf.run_body(s)]
                want = {self.features_of(b) for b in man_bodies
                        if "cargo build --release -p manta-cli" in b}
                self.assertEqual(len(want), 1, f"validate-tag builds with several feature sets: {want}")
                want = next(iter(want))
                build = wf.job_block(wf.read(name), "build")
                checked = 0
                for step in wf.steps(build):
                    if wf.field(step, "name") in ("Build (native)", "Build (cross)"):
                        self.assertEqual(
                            self.features_of(executable(wf.run_body(step))), want,
                            "the man-page build's --features must equal every build leg's")
                        checked += 1
                self.assertEqual(checked, 2, "build lost its native/cross build steps")

    def test_build_downloads_the_man_pages_before_packaging(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                build = wf.job_block(wf.read(name), "build")
                self.assertEqual(wf.field(build, "needs"), "validate-tag")
                steps = wf.steps(build)
                pkg = [i for i, s in enumerate(steps)
                       if "scripts/package-release.py" in executable(wf.run_body(s))]
                self.assertEqual(len(pkg), 1)
                downloads = [i for i, s in enumerate(steps)
                             if (wf.field(s, "uses") or "").startswith("actions/download-artifact@")
                             and wf.field(wf.sub(s, "with"), "name") == "man-pages"]
                self.assertEqual(len(downloads), 1, "expected exactly one man-pages download step")
                self.assertLess(downloads[0], pkg[0], "the download must come before Package")
                self.assertEqual(wf.field(wf.sub(steps[downloads[0]], "with"), "path"), "man-pages")
                self.assertIn("--man-dir man-pages", executable(wf.run_body(steps[pkg[0]])))

    def test_build_soapy_renders_man_pages_from_the_binary_it_packages(self):
        # MAN-82: validate-tag's pages describe the hpsdr-only binary; the Soapy archive's
        # pages must come from its own executable, after its build and before Package.
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                steps = wf.steps(wf.job_block(wf.read(name), SOAPY_JOB))
                bodies = [executable(wf.run_body(s)).strip() for s in steps]
                build = [i for i, b in enumerate(bodies) if b.startswith("cargo build --release")]
                man = [i for i, b in enumerate(bodies)
                       if b == f"{SOAPY_BINARY} man-pages --out {SOAPY_MAN_DIR}"]
                pkg = [i for i, b in enumerate(bodies) if "scripts/package-release.py" in b]
                self.assertEqual((len(build), len(man), len(pkg)), (1, 1, 1))
                self.assertLess(build[0], man[0])
                self.assertLess(man[0], pkg[0])
                self.assertEqual(self.features_of(bodies[build[0]]), "hpsdr,soapy")
                self.assertIn(f"--target {SOAPY_ENTRY['target']}", bodies[build[0]])
                self.assertIn(f"--man-dir {SOAPY_MAN_DIR}", bodies[pkg[0]])
                self.assertIn(f'--binary "target/{SOAPY_ENTRY["target"]}/release/${{BIN_NAME}}"',
                              bodies[pkg[0]])
                downloads = [s for s in steps
                             if (wf.field(s, "uses") or "").startswith("actions/download-artifact@")]
                self.assertEqual(downloads, [], f"{SOAPY_JOB} must not consume the default man pages")
                self.assertNotIn("--man-dir man-pages\n", "\n".join(bodies) + "\n")

    def test_man_pages_artifact_is_never_a_release_asset(self):
        # D7's naming rule: the release job downloads `manta-*` and publishes
        # everything it downloads, so `man-pages` must not match that glob.
        self.assertFalse(fnmatch.fnmatchcase("man-pages", "manta-*"))
        release = wf.job_block(wf.read("release-publish.yml"), "release")
        downloads = [s for s in wf.steps(release)
                     if (wf.field(s, "uses") or "").startswith("actions/download-artifact@")]
        self.assertEqual(len(downloads), 1, "release job's artifact download changed")
        self.assertEqual(wf.field(wf.sub(downloads[0], "with"), "pattern"), "manta-*")


class BuildSoapyJobTests(unittest.TestCase):
    """MAN-82: the extra Linux x86_64 archive with the Soapy input, built on the Ubuntu release
    it supports, beside -- not inside -- the unchanged five-leg build matrix."""

    def job(self, name):
        block = wf.job_block(wf.read(name), SOAPY_JOB)
        self.assertTrue(block, f"{name} has no {SOAPY_JOB} job")
        return block

    def test_job_needs_validate_tag_on_pinned_ubuntu_24_04(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                job = self.job(name)
                self.assertEqual(wf.field(job, "needs"), "validate-tag")
                self.assertEqual(wf.field(job, "runs-on"), "ubuntu-24.04")
                for key in ("if", "environment", "permissions", "continue-on-error", "strategy"):
                    self.assertIsNone(wf.field(job, key), f"{SOAPY_JOB} must not set {key}:")

    def test_job_builds_with_both_features_from_the_lockfile(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                bodies = [executable(wf.run_body(s)).strip() for s in wf.steps(self.job(name))]
                builds = [b for b in bodies if "cargo build" in b or "cross build" in b]
                self.assertEqual(builds, ["cargo build --release --locked -p manta-cli --features hpsdr,soapy "
                                          f"--target {SOAPY_ENTRY['target']}"])

    def test_job_installs_link_dependencies_without_recommends(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                bodies = [executable(wf.run_body(s)) for s in wf.steps(self.job(name))]
                installs = [b for b in bodies if "apt-get install" in b and "docker run" not in b]
                self.assertEqual(len(installs), 1)
                self.assertIn("--no-install-recommends", installs[0])
                for package in ("pkg-config", "libasound2-dev", "libsoapysdr-dev"):
                    self.assertRegex(installs[0], rf"(?<![\w-]){re.escape(package)}(?![\w-])")
                self.assertNotIn("soapysdr0.8-module", installs[0], "device modules are runtime choices")

    def test_job_reuses_build_s_pinned_actions(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                pinned = {(wf.field(s, "uses") or "").split("@")[0]: wf.field(s, "uses")
                          for s in wf.steps(wf.job_block(wf.read(name), "build")) if wf.field(s, "uses")}
                uses = [wf.field(s, "uses") for s in wf.steps(self.job(name)) if wf.field(s, "uses")]
                self.assertTrue(uses)
                for action in uses:
                    self.assertEqual(action, pinned.get(action.split("@")[0]), f"{action} differs from build's")

    def test_default_build_matrix_is_unchanged(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                text = wf.read(name)
                self.assertEqual({e["artifact"] for e in matrix_include(text)}, DEFAULT_ARTIFACTS)
                for step in wf.steps(wf.job_block(text, "build")):
                    if wf.field(step, "name") in ("Build (native)", "Build (cross)"):
                        self.assertRegex(executable(wf.run_body(step)), r"--features hpsdr(\s|$)")


class CiFullTests(unittest.TestCase):
    def runs(self, module):
        steps = wf.steps(wf.job_block(wf.read("ci-full.yml"), "test"))
        return [s for s in steps if f"-m unittest scripts.tests.{module} -v" in executable(wf.run_body(s))]

    def test_packaging_tests_run_on_unix_legs(self):
        found = self.runs("test_packaging")
        self.assertEqual(len(found), 1, "ci-full.yml test job must run scripts.tests.test_packaging")
        self.assertEqual(wf.field(found[0], "if"), "runner.os != 'Windows'")

    def test_archive_tests_run_on_every_leg(self):
        found = self.runs("test_package_release")
        self.assertEqual(len(found), 1, "ci-full.yml test job must run scripts.tests.test_package_release")
        self.assertIsNone(wf.field(found[0], "if"), "Windows keeps zip coverage: no if: gate")
        self.assertEqual(wf.field(found[0], "shell"), "bash")


if __name__ == "__main__":
    unittest.main()
