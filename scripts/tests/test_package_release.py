"""Tests for scripts/package-release.py and the release workflows' Package steps (MAN-268).

stdlib unittest only. Builds real tar.gz and zip archives from the repository's files and a fake
binary, then executes the workflows' own Package run: bodies against the helper.
Run: python3 -m unittest scripts.tests.test_package_release -v
"""
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
)
EXECUTABLE_KIT = {"packaging/launchd/create-service-account.sh", "packaging/launchd/rotate-log.sh"}
FAKE_BINARY = b"\x7fELF fake manta\x00\x01\x02"
RELEASE_WORKFLOWS = ("release.yml", "release-publish.yml")
OLD_PACKAGING = ("tar -C dist", "Compress-Archive")


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
    def assert_release_archive(self, path, artifact, binary_name, binary_bytes):
        members = read_archive(path)
        self.assertEqual({n.split("/")[0] for n in members}, {artifact}, "exactly one top-level directory")
        self.assertEqual(members.get(artifact, ("missing",))[0], "dir")
        for name in members:
            self.assertFalse(name.startswith("/") or ".." in name.split("/"), name)
        files = {n[len(artifact) + 1:]: v for n, v in members.items() if v[0] != "dir"}
        self.assertEqual(sorted(files), sorted((binary_name,) + KIT))
        self.assertEqual({v[0] for v in files.values()}, {"file"}, "only regular files")
        self.assertEqual(files[binary_name][2], binary_bytes)
        for rel in KIT:
            self.assertEqual(files[rel][2], repo_bytes(rel), rel)
        for rel in (binary_name,) + tuple(EXECUTABLE_KIT):
            self.assertEqual(files[rel][1] & 0o755, 0o755, f"{rel} lost its executable mode")
        with tempfile.TemporaryDirectory() as tmp:
            extract(path, tmp)
            for rel in (binary_name,) + KIT:
                with open(os.path.join(tmp, artifact, *rel.split("/")), "rb") as f:
                    self.assertEqual(f.read(), binary_bytes if rel == binary_name else repo_bytes(rel), rel)
            if os.name == "posix" and path.endswith(".tar.gz"):
                for rel in (binary_name,) + tuple(EXECUTABLE_KIT):
                    self.assertTrue(os.access(os.path.join(tmp, artifact, *rel.split("/")), os.X_OK), rel)


class HelperArchiveTests(ArchiveAssertions):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = os.path.join(self._tmp.name, "release work")  # a space in every path
        self.out = os.path.join(self.tmp, "out dir")
        os.makedirs(self.out)

    def tearDown(self):
        self._tmp.cleanup()

    def fake_binary(self, name):
        path = os.path.join(self.tmp, "build output", name)
        write_file(path, FAKE_BINARY)
        return path

    def test_tar_gz_contains_the_binary_readme_licences_and_kit(self):
        artifact = "manta-linux-x86_64"
        proc = run_helper("--binary", self.fake_binary("manta"), "--artifact", artifact,
                          "--format", "tar.gz", "--out-dir", self.out)
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
                          "--format", "zip", "--out-dir", self.out)
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
                                  "--format", fmt, "--out-dir", self.out)
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
                                          "--format", fmt, "--out-dir", out, root=root)
                        self.assertNotEqual(proc.returncode, 0, f"archive built without {rel}")
                        self.assertIn(rel.split("/")[-1], proc.stderr)
                        self.assertEqual(os.listdir(out), [], "no archive or partial file may be left")

    def test_missing_binary_fails_without_writing_an_archive(self):
        proc = run_helper("--binary", os.path.join(self.tmp, "no such dir", "manta"),
                          "--artifact", "manta-linux-x86_64", "--format", "tar.gz", "--out-dir", self.out)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("no such dir", proc.stderr)
        self.assertEqual(os.listdir(self.out), [])

    def test_complete_copy_of_the_kit_packages_from_root(self):
        # Guards the missing-asset test above: the same copied root succeeds when nothing is removed.
        root = os.path.join(self.tmp, "repo copy")
        copy_kit(root)
        proc = run_helper("--binary", self.fake_binary("manta"), "--artifact", "manta-linux-arm64",
                          "--format", "tar.gz", "--out-dir", self.out, root=root)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assert_release_archive(os.path.join(self.out, "manta-linux-arm64.tar.gz"), "manta-linux-arm64",
                                    "manta", FAKE_BINARY)

    def test_artifact_must_be_one_path_component(self):
        for artifact in ("", "..", "dist/manta-linux-x86_64", "..\\manta"):
            with self.subTest(artifact=artifact):
                proc = run_helper("--binary", self.fake_binary("manta"), "--artifact", artifact,
                                  "--format", "tar.gz", "--out-dir", self.out)
                self.assertEqual(proc.returncode, 2, proc.stderr)
                self.assertEqual(os.listdir(self.out), [])


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


class WorkflowPackageStepTests(ArchiveAssertions):
    def test_each_release_workflow_packages_through_the_helper(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name):
                found = package_steps(name)
                self.assertEqual(len(found), 1, f"{name}: expected one step invoking scripts/package-release.py")
                job, step = found[0]
                self.assertEqual(job, "build")
                self.assertEqual(wf.field(step, "shell"), "bash")
                self.assertIsNone(wf.field(step, "if"), "the Package step must run on every OS")
                body = executable(wf.run_body(step))
                for arg in ("--binary ", '--artifact "${{ matrix.artifact }}"', "--format ", "--out-dir "):
                    self.assertIn(arg, body)
                self.assertIn("matrix.target", body)

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

    @unittest.skipIf(shutil.which("bash") is None, "bash is not installed")
    def test_package_step_builds_every_matrix_archive(self):
        bash = shutil.which("bash")
        for name in RELEASE_WORKFLOWS:
            text = wf.read(name)
            bin_name = re.search(r"^  BIN_NAME: (\S+)$", text, re.M).group(1)
            found = package_steps(name)
            self.assertEqual(len(found), 1, f"{name}: expected one step invoking scripts/package-release.py")
            body = wf.run_body(found[0][1])
            entries = matrix_include(text)
            self.assertIn(("x86_64-unknown-linux-gnu", ""),
                          {(e["target"], e.get("exe_ext", "")) for e in entries})
            self.assertIn(("x86_64-pc-windows-msvc", ".exe"),
                          {(e["target"], e.get("exe_ext", "")) for e in entries})
            for entry in entries:
                with self.subTest(workflow=name, target=entry["target"]):
                    self._run_step(bash, body, bin_name, entry)

    def _run_step(self, bash, body, bin_name, entry):
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
