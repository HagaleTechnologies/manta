"""Tests for scripts/check-soapy-release.py and the release workflow's SoapySDR jobs (MAN-82, MAN-539).

stdlib unittest only. Builds manta-linux-<arch>-soapy.tar.gz fixtures around a fake `manta` shell
script whose answers each test chooses, runs the verifier against them, drives the verifier's
--docker-image mode through a fake `docker` on PATH, and reads the release workflow as text through
test_ci_trust_boundary.py's helpers: the build-soapy and build-soapy-arm64 archive jobs and the
Docker jobs' pre-push image check. The real binary's check is the release job itself;
docs/RUNBOOKS/release.md says how to run the verifier on a local build.
Run: python3 -m unittest scripts.tests.test_soapy_release -v
"""
import importlib.util
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
VERIFIER = os.path.join(ROOT, "scripts", "check-soapy-release.py")
_spec = importlib.util.spec_from_file_location("_ci_workflow_text", os.path.join(HERE, "test_ci_trust_boundary.py"))
wf = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(wf)

ARTIFACT = "manta-linux-x86_64-soapy"
ARM64_ARTIFACT = "manta-linux-arm64-soapy"
RELEASE_WORKFLOWS = ("release-publish.yml",)
# One row per SoapySDR archive job (MAN-82, MAN-539). `runtime` is the clean operator image the
# archive is verified in; `alsa` is that distribution's ALSA runtime package; `host_verify` says
# whether the job also runs the verifier on the build runner itself.
SOAPY_JOBS = {
    "build-soapy": {"artifact": ARTIFACT, "runtime": "ubuntu:24.04",
                    "alsa": "libasound2t64", "host_verify": True},
    "build-soapy-arm64": {"artifact": ARM64_ARTIFACT, "runtime": "debian:bookworm",
                          "alsa": "libasound2", "host_verify": False},
}
SOAPY_TEST = "cargo test --release --locked -p manta-input --features soapy --lib soapy::tests"
IMAGE_JOBS = ("docker-publish-release", "docker-build-dispatch")
CHECK_IMAGE = "manta-soapy-check:local"
CHECK_PLATFORM = "linux/arm64"
IMAGE_VERIFY = (f"python3 scripts/check-soapy-release.py --docker-image {CHECK_IMAGE} --platform {CHECK_PLATFORM}"
                ' --expect-git-sha "$GITHUB_SHA"')
# MAN-537: every image build passes the release commit, the same value in the check and the push.
IMAGE_BUILD_ARGS = "MANTA_GIT_SHA=${{ github.sha }}"
# The commit the fake `manta --version` reports.
FAKE_SHA = "0123456789ab"
THREE_PLATFORMS = "linux/amd64,linux/arm64,linux/arm/v7"
FLAGS = ("--soapy-driver", "--soapy-freq-hz", "--soapy-rate-hz", "--soapy-gain")
HELP = "Usage: manta run [OPTIONS]\n" + "".join(f"      {f} <X>\n" for f in FLAGS + ("--hpsdr-host",))
ROFF_FLAGS = "".join(f"\\fB{f.replace('-', chr(92) + '-')}\\fR\n" for f in FLAGS)
PAGES = {
    "manta.1": ".TH manta 1\n",
    "manta-run.1": ".TH manta-run 1\n" + ROFF_FLAGS,
    "manta-check.1": ".TH manta-check 1\n" + ROFF_FLAGS,
    "manta-devices.1": ".TH manta-devices 1\n",
}
OK_DEVICES = {"audio": {"status": "ok", "devices": [], "error": None},
              "soapy": {"status": "ok", "devices": [], "error": None}}
NO_MATCH = "error: cannot open source: Other: SoapySDR::Device::make() no match"
UNEXPECTED = ("error: unexpected argument '--soapy-driver' found\n\n"
              "Usage: manta check [OPTIONS] [SOURCE]\n")
LOADER = ("./manta: error while loading shared libraries: libSoapySDR.so.0.8: "
          "cannot open shared object file: No such file or directory")
# What each installed module answers with no device attached (MAN-539 planning runs, E1).
MODULES = {
    "rtlsdr": ("error: cannot open source: Other: No RTL-SDR devices found!", 1),
    "airspy": ("error: cannot open source: Other: Unable to open AirSpy device", 1),
    "hackrf": ("error: cannot open source: Other: no hackrf device matches", 1),
}
IMAGE_OK = [
    "OK: manta 0.1.0 (git 0123456789ab; features: soapy)",
    "OK: run/listen/check --help offer --soapy-driver, --soapy-freq-hz, --soapy-rate-hz, --soapy-gain "
    "and --hpsdr-host",
    "OK: nonexistent driver: exit 1, SoapySDR::Device::make() no match",
    "OK: devices --json: Soapy enumeration ok, 0 device(s)",
    "OK: modules rtlsdr, airspy, hackrf load: each driver fails with its own error, not no match",
]


def q(text):
    return "'" + text.replace("'", "'\\''") + "'"


def fake_manta(version="manta 0.1.0 (git 0123456789ab; features: soapy)", help_text=HELP,
               driver=(NO_MATCH, 1), devices=(json.dumps(OK_DEVICES), 0), version_sleep=0,
               loader=None, modules=MODULES):
    """A POSIX sh stand-in for the release binary. `loader` makes every call fail as the
    dynamic loader does when libSoapySDR is missing."""
    if loader is not None:
        return f"#!/bin/sh\nprintf '%s\\n' {q(loader[0])} >&2\nexit {loader[1]}\n"
    return "#!/bin/sh\n" + manta_answers(version, help_text, driver, devices, version_sleep, modules)


def manta_answers(version, help_text, driver, devices, version_sleep, modules):
    """The sh `case` that answers manta's arguments in $1.., shared by the fake manta and docker."""
    module_answers = "".join(
        f"""    if [ "$1" = check ] && [ "$2" = --soapy-driver ] && [ "$3" = driver={name} ]; then
      printf '%s\\n' {q(text)} >&2
      exit {code}
    fi
""" for name, (text, code) in modules.items())
    return f"""case "$1" in
  --version)
    sleep {version_sleep}
    printf '%s\\n' {q(version)}
    exit 0 ;;
  run|listen|check)
    if [ "$2" = "--help" ]; then printf '%s' {q(help_text)}; exit 0; fi
    if [ "$1" = check ] && [ "$2" = --soapy-driver ] && [ "$3" = driver=man82_nonexistent ]; then
      printf '%s\\n' {q(driver[0])} >&2
      exit {driver[1]}
    fi
{module_answers}    ;;
  devices)
    if [ "$2" = "--json" ]; then printf '%s\\n' {q(devices[0])}; exit {devices[1]}; fi ;;
esac
echo "fake manta: unexpected arguments: $*" >&2
exit 99
"""


def fake_docker(record, **answers):
    """A POSIX sh stand-in for `docker`: it appends its argv to RECORD, insists on
    `run --rm --platform <p> <image>` and answers the rest as fake_manta does with ANSWERS."""
    kwargs = dict(version="manta 0.1.0 (git 0123456789ab; features: soapy)", help_text=HELP,
                  driver=(NO_MATCH, 1), devices=(json.dumps(OK_DEVICES), 0), version_sleep=0,
                  modules=MODULES)
    kwargs.update(answers)
    return f"""#!/bin/sh
printf '%s\\n' "$*" >> {q(record)}
if [ "$1" != run ] || [ "$2" != --rm ] || [ "$3" != --platform ] || [ -z "$4" ] || [ -z "$5" ]; then
  echo "fake docker: unexpected arguments: $*" >&2
  exit 125
fi
shift 5
""" + manta_answers(**kwargs)


def add(tar, name, data=b"", mode=0o644, kind=tarfile.REGTYPE, linkname=""):
    info = tarfile.TarInfo(name)
    info.type = kind
    info.mode = mode
    info.linkname = linkname
    if kind == tarfile.REGTYPE:
        info.size = len(data)
        tar.addfile(info, io.BytesIO(data))
    else:
        tar.addfile(info)


def build_archive(directory, binary=None, binary_mode=0o755, pages=PAGES, root=ARTIFACT,
                  name=ARTIFACT + ".tar.gz", extra=()):
    """Write a release-shaped archive; returns its path. `extra` is (name, kwargs) members."""
    path = os.path.join(directory, name)
    with tarfile.open(path, "w:gz") as tar:
        add(tar, root, mode=0o755, kind=tarfile.DIRTYPE)
        if binary is not None:
            add(tar, f"{root}/manta", binary.encode("utf-8"), mode=binary_mode)
        add(tar, f"{root}/README.md", b"# manta\n")
        for page, text in pages.items():
            add(tar, f"{root}/man/man1/{page}", text.encode("utf-8"))
        for member, kwargs in extra:
            add(tar, member, **kwargs)
    return path


def verify(archive, *args):
    return subprocess.run([sys.executable, VERIFIER, *args, archive], capture_output=True, text=True)


@unittest.skipUnless(os.name == "posix", "the fake manta is a POSIX shell script")
class VerifierTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = os.path.join(self._tmp.name, "release work")  # a space in every path
        os.makedirs(self.tmp)

    def tearDown(self):
        self._tmp.cleanup()

    def assert_fails(self, proc, needle):
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("FAIL", proc.stderr)
        self.assertIn(needle, proc.stderr)
        self.assertNotIn("OK:", proc.stdout)

    def test_a_complete_archive_passes_every_check(self):
        proc = verify(build_archive(self.tmp, fake_manta()))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("OK: manta 0.1.0 (git 0123456789ab; features: soapy)", proc.stdout)
        self.assertIn("OK: nonexistent driver: exit 1, SoapySDR::Device::make() no match", proc.stdout)
        self.assertIn("OK: devices --json: Soapy enumeration ok, 0 device(s)", proc.stdout)

    def test_an_arm64_archive_passes_with_its_own_root(self):
        # MAN-539: the root directory comes from the file name, so the arm64 archive needs no
        # verifier change.
        archive = build_archive(self.tmp, fake_manta(), root=ARM64_ARTIFACT, name=ARM64_ARTIFACT + ".tar.gz")
        proc = verify(archive)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("OK: devices --json: Soapy enumeration ok, 0 device(s)", proc.stdout)
        wrong = build_archive(self.tmp, fake_manta(), root=ARTIFACT, name=ARM64_ARTIFACT + ".tar.gz")
        self.assert_fails(verify(wrong), ARM64_ARTIFACT)

    def test_feature_order_and_extra_features_are_accepted(self):
        proc = verify(build_archive(self.tmp, fake_manta(
            version="manta 9.9.9 (git ffffffffffff; features: soapy,extra,hpsdr)")))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_missing_binary_fails(self):
        self.assert_fails(verify(build_archive(self.tmp, None)), f"{ARTIFACT}/manta")

    def test_non_executable_binary_fails(self):
        self.assert_fails(verify(build_archive(self.tmp, fake_manta(), binary_mode=0o644)), "not executable")

    def test_missing_man_page_fails(self):
        for page in ("manta.1", "manta-check.1", "manta-devices.1"):
            with self.subTest(page=page):
                pages = {k: v for k, v in PAGES.items() if k != page}
                self.assert_fails(verify(build_archive(self.tmp, fake_manta(), pages=pages)),
                                  f"man/man1/{page}")

    def test_default_build_man_pages_fail(self):
        # validate-tag's default-build pages: right names, no Soapy flags.
        pages = dict(PAGES, **{"manta-check.1": ".TH manta-check 1\n\\fB\\-\\-hpsdr\\-host\\fR\n"})
        self.assert_fails(verify(build_archive(self.tmp, fake_manta(), pages=pages)),
                          "man/man1/manta-check.1 does not document --soapy-driver")

    def test_binary_without_the_soapy_feature_fails(self):
        for version in ("manta 0.1.0 (git 0123456789ab; features: hpsdr)",
                        "manta 0.1.0 (git 0123456789ab; features: none)", "manta 0.1.0"):
            with self.subTest(version=version):
                self.assert_fails(verify(build_archive(self.tmp, fake_manta(version=version))),
                                  "does not list features")

    def test_help_missing_a_soapy_flag_fails(self):
        for flag in FLAGS + ("--hpsdr-host",):
            with self.subTest(flag=flag):
                help_text = HELP.replace(f"      {flag} <X>\n", "")
                self.assert_fails(verify(build_archive(self.tmp, fake_manta(help_text=help_text))),
                                  f"does not offer {flag}")

    def test_a_longer_flag_does_not_stand_in_for_a_missing_one(self):
        help_text = HELP.replace("--soapy-gain <X>", "--soapy-gain-db <X>")
        self.assert_fails(verify(build_archive(self.tmp, fake_manta(help_text=help_text))),
                          "does not offer --soapy-gain")

    def test_driver_flag_rejected_by_argument_parsing_fails(self):
        self.assert_fails(verify(build_archive(self.tmp, fake_manta(driver=(UNEXPECTED, 2)))),
                          "unexpected argument '--soapy-driver'")

    def test_driver_check_needs_the_library_no_match_error(self):
        for driver in ((LOADER, 127), ("error: cannot open source: something else", 1),
                       (NO_MATCH, 0), (NO_MATCH, 134)):
            with self.subTest(driver=driver):
                self.assert_fails(verify(build_archive(self.tmp, fake_manta(driver=driver))),
                                  "SoapySDR::Device::make() no match")

    def test_devices_soapy_section_must_have_enumerated(self):
        cases = (
            ({"audio": OK_DEVICES["audio"], "soapy": {"status": "disabled", "devices": [], "error": None}}, 0),
            ({"audio": OK_DEVICES["audio"], "soapy": {"status": "error", "devices": [], "error": "boom"}}, 1),
            ({"audio": OK_DEVICES["audio"]}, 0),
        )
        for report, code in cases:
            with self.subTest(report=report):
                devices = (json.dumps(report), code)
                self.assert_fails(verify(build_archive(self.tmp, fake_manta(devices=devices))), "Soapy")
        self.assert_fails(verify(build_archive(self.tmp, fake_manta(devices=("not json", 0)))),
                          "printed no Soapy section")
        self.assert_fails(verify(build_archive(self.tmp, fake_manta(devices=(json.dumps(OK_DEVICES), 101)))),
                          "devices --json` failed")

    def test_audio_enumeration_failure_alone_is_not_a_soapy_failure(self):
        report = dict(OK_DEVICES, audio={"status": "error", "devices": [], "error": "no ALSA"})
        report["soapy"] = {"status": "ok", "devices": [{"args": "driver=rtlsdr"}], "error": None}
        proc = verify(build_archive(self.tmp, fake_manta(devices=(json.dumps(report), 1))))
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("1 device(s)", proc.stdout)

    def test_a_hung_binary_fails_at_the_timeout(self):
        proc = verify(build_archive(self.tmp, fake_manta(version_sleep=30)), "--timeout", "0.5")
        self.assert_fails(proc, "did not finish within 0.5 s")

    def test_members_outside_the_archive_root_are_refused_before_extraction(self):
        outside = os.path.join(self._tmp.name, "escaped")
        cases = (
            ("traversal", [(f"{ARTIFACT}/../escaped", {"data": b"x"})], {}),
            ("absolute", [(outside, {"data": b"x"})], {}),
            ("other root", [("manta-linux-x86_64/manta", {"data": b"x"})], {}),
            ("symlink", [(f"{ARTIFACT}/link", {"kind": tarfile.SYMTYPE, "linkname": "/etc/passwd"})], {}),
            ("hardlink", [(f"{ARTIFACT}/hard", {"kind": tarfile.LNKTYPE, "linkname": "../escaped"})], {}),
            ("wrong root", [], {"root": "manta-linux-x86_64"}),
        )
        for label, extra, kwargs in cases:
            with self.subTest(case=label):
                archive = build_archive(self.tmp, fake_manta(), extra=extra, **kwargs)
                proc = verify(archive)
                self.assert_fails(proc, ARTIFACT)
                self.assertFalse(os.path.exists(outside), "a member escaped the temporary directory")

    def test_not_a_tar_gz_fails(self):
        path = os.path.join(self.tmp, ARTIFACT + ".tar.gz")
        with open(path, "wb") as f:
            f.write(b"not an archive")
        self.assert_fails(verify(path), "cannot extract")
        self.assert_fails(verify(os.path.join(self.tmp, ARTIFACT + ".zip")), "not a .tar.gz archive")

    def test_archive_bytes_are_left_untouched(self):
        archive = build_archive(self.tmp, fake_manta())
        with open(archive, "rb") as f:
            before = f.read()
        proc = verify(archive)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        with open(archive, "rb") as f:
            self.assertEqual(f.read(), before)
        self.assertEqual(sorted(os.listdir(self.tmp)), [ARTIFACT + ".tar.gz"], "nothing extracted beside it")

    def test_missing_library_mode_wants_the_loader_error(self):
        proc = verify(build_archive(self.tmp, fake_manta(loader=(LOADER, 127))), "--expect-missing-soapy-library")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn("libSoapySDR.so.0.8", proc.stdout)

    def test_missing_library_mode_refuses_any_other_outcome(self):
        cases = (
            fake_manta(),  # starts: the library was there after all
            fake_manta(loader=("Segmentation fault", 139)),
            fake_manta(loader=(LOADER, 1)),
            fake_manta(loader=("./manta: error while loading shared libraries: libasound.so.2: "
                               "cannot open shared object file: No such file or directory", 127)),
        )
        for binary in cases:
            with self.subTest(binary=binary[:80]):
                self.assert_fails(verify(build_archive(self.tmp, binary), "--expect-missing-soapy-library"),
                                  "libSoapySDR.so.0.8")

    def test_usage_errors_exit_2(self):
        archive = build_archive(self.tmp, fake_manta())
        for args in ((), ("--timeout", "0", archive), ("--no-such-flag", archive)):
            with self.subTest(args=args):
                proc = subprocess.run([sys.executable, VERIFIER, *args], capture_output=True, text=True)
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)


@unittest.skipUnless(os.name == "posix", "the fake docker is a POSIX shell script")
class ImageModeTests(unittest.TestCase):
    """MAN-539: --docker-image runs every manta call as `docker run --rm --platform P IMAGE`."""

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = os.path.join(self._tmp.name, "image work")  # a space in every path
        os.makedirs(self.tmp)
        self.bin = os.path.join(self.tmp, "bin")
        os.makedirs(self.bin)
        self.record = os.path.join(self.tmp, "docker-argv")

    def tearDown(self):
        self._tmp.cleanup()

    def check(self, *args, **answers):
        docker = os.path.join(self.bin, "docker")
        with open(docker, "w", encoding="utf-8") as f:
            f.write(fake_docker(self.record, **answers))
        os.chmod(docker, 0o755)
        env = dict(os.environ, PATH=self.bin + os.pathsep + os.environ.get("PATH", ""))
        argv = args or ("--docker-image", CHECK_IMAGE, "--platform", CHECK_PLATFORM)
        return subprocess.run([sys.executable, VERIFIER, *argv], capture_output=True, text=True, env=env)

    def recorded(self):
        with open(self.record, encoding="utf-8") as f:
            return f.read().splitlines()

    def assert_fails(self, proc, needle):
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("FAIL", proc.stderr)
        self.assertIn(needle, proc.stderr)
        self.assertNotIn("OK:", proc.stdout)

    def test_a_soapy_image_passes_every_check(self):
        proc = self.check()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(proc.stdout.splitlines(), IMAGE_OK)
        calls = self.recorded()
        self.assertTrue(calls)
        for call in calls:
            self.assertTrue(call.startswith(f"run --rm --platform {CHECK_PLATFORM} {CHECK_IMAGE} "), call)
        for name in MODULES:
            self.assertIn(f"{CHECK_IMAGE} check --soapy-driver driver={name} --soapy-freq-hz 14025000 "
                          "--soapy-rate-hz 1024000", "\n".join(calls))

    def test_a_missing_module_fails_naming_its_driver(self):
        modules = dict(MODULES, rtlsdr=(NO_MATCH, 1))
        proc = self.check(modules=modules)
        self.assert_fails(proc, "rtlsdr")
        self.assertIn("no match", proc.stderr)

    def test_a_module_that_opens_or_fails_otherwise_fails(self):
        for answer in ((MODULES["airspy"][0], 0), ("error: unexpected argument", 2),
                       ("error: something else", 1)):
            with self.subTest(answer=answer):
                self.assert_fails(self.check(modules=dict(MODULES, airspy=answer)), "airspy")

    def test_an_image_without_the_soapy_feature_fails(self):
        self.assert_fails(self.check(version="manta 0.1.0 (git 0123456789ab; features: none)"),
                          "does not list features")

    def test_the_image_shares_the_archive_checks(self):
        self.assert_fails(self.check(driver=(UNEXPECTED, 2)), "unexpected argument '--soapy-driver'")
        bad = {"audio": OK_DEVICES["audio"], "soapy": {"status": "disabled", "devices": [], "error": None}}
        self.assert_fails(self.check(devices=(json.dumps(bad), 0)), "Soapy")

    def check_commit(self, expected, **answers):
        return self.check("--docker-image", CHECK_IMAGE, "--platform", CHECK_PLATFORM,
                          "--expect-git-sha", expected, **answers)

    def test_the_expected_commit_passes(self):
        # MAN-537: a 12-hex value is compared verbatim.
        proc = self.check_commit(FAKE_SHA)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(proc.stdout.splitlines(),
                         IMAGE_OK[:1] + [f"OK: image commit {FAKE_SHA} matches --expect-git-sha"]
                         + IMAGE_OK[1:])

    def test_a_full_commit_name_matches_its_lowercase_12_hex_prefix(self):
        # build.rs cuts a 40- or 64-hex override to 12 lower-case characters; so does the check.
        for full in ((FAKE_SHA + "cdef0123456789abcdef01234567").upper(),
                     FAKE_SHA + "cd" * 26):
            with self.subTest(full=full):
                proc = self.check_commit(full)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertIn(f"OK: image commit {FAKE_SHA} matches --expect-git-sha", proc.stdout)

    def test_an_image_of_another_commit_fails_naming_both(self):
        proc = self.check_commit("fedcba987654")
        self.assert_fails(proc, "fedcba987654")
        self.assertIn(FAKE_SHA, proc.stderr)

    def test_an_image_that_reports_unknown_fails(self):
        proc = self.check_commit(FAKE_SHA, version="manta 0.1.0 (git unknown; features: soapy)")
        self.assert_fails(proc, "unknown")
        self.assertIn(FAKE_SHA, proc.stderr)

    def test_a_version_line_without_a_git_field_fails(self):
        proc = self.check_commit(FAKE_SHA, version="manta 0.1.0 (features: soapy)")
        self.assert_fails(proc, "no git commit")

    def test_usage_errors_exit_2(self):
        archive = build_archive(self.tmp, fake_manta())
        cases = (
            (archive, "--docker-image", CHECK_IMAGE, "--platform", CHECK_PLATFORM),
            ("--timeout", "5"),
            ("--docker-image", CHECK_IMAGE),
            ("--platform", CHECK_PLATFORM, archive),
            ("--expect-missing-soapy-library", "--docker-image", CHECK_IMAGE, "--platform", CHECK_PLATFORM),
            # MAN-537: the commit check is image mode only, and needs a value.
            ("--expect-git-sha", FAKE_SHA, archive),
            ("--docker-image", CHECK_IMAGE, "--platform", CHECK_PLATFORM, "--expect-git-sha", ""),
        )
        for args in cases:
            with self.subTest(args=args):
                proc = self.check(*args)
                self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
                self.assertFalse(os.path.exists(self.record), "a usage error must not run docker")


def build_soapy_steps(name, job="build-soapy"):
    return wf.steps(wf.job_block(wf.read(name), job))


def executable(body):
    return "\n".join(wf.code_lines(body or ""))


def index_of(steps, pred, what, job="build-soapy"):
    found = [i for i, s in enumerate(steps) if pred(s)]
    if len(found) != 1:
        raise AssertionError(f"{job}: expected one {what} step, found {len(found)}")
    return found[0]


def is_upload(step):
    return (wf.field(step, "uses") or "").startswith("actions/upload-artifact@")


class WorkflowWiringTests(unittest.TestCase):
    def each_job(self):
        for name in RELEASE_WORKFLOWS:
            for job, row in SOAPY_JOBS.items():
                with self.subTest(workflow=name, job=job):
                    yield name, job, row

    def runtime_body(self, name, job, row):
        # Selected by the clean runtime's image name: build-soapy-arm64's build step is a
        # `docker run` too, in rust:1-slim-bookworm.
        steps = build_soapy_steps(name, job)
        i = index_of(steps, lambda s: "docker run" in executable(wf.run_body(s))
                     and re.search(r"(?<![\w:/.-])" + re.escape(row["runtime"]) + r"\s",
                                   executable(wf.run_body(s))) is not None,
                     "clean-runtime", job)
        return i, executable(wf.run_body(steps[i]))

    def test_the_workflow_verifies_the_archive_before_upload(self):
        for name, job, row in self.each_job():
            steps = build_soapy_steps(name, job)
            self.assertTrue(steps, f"{name} has no {job} job")
            package = index_of(steps, lambda s: "scripts/package-release.py" in executable(wf.run_body(s)),
                               "Package", job)
            runtime, body = self.runtime_body(name, job, row)
            upload = index_of(steps, is_upload, "upload", job)
            self.assertLess(package, runtime)
            self.assertLess(runtime, upload, "a failed smoke check must stop the upload")
            smoke = [runtime, upload]
            verify_line = f"python3 scripts/check-soapy-release.py {row['artifact']}.tar.gz"
            if row["host_verify"]:
                host = index_of(steps, lambda s: executable(wf.run_body(s)).strip() == verify_line,
                                "verifier", job)
                self.assertLess(package, host)
                self.assertLess(host, runtime)
                smoke.insert(0, host)
            else:
                outside = [i for i, s in enumerate(steps)
                           if "check-soapy-release.py" in executable(wf.run_body(s))
                           and "docker run" not in executable(wf.run_body(s))]
                self.assertEqual(outside, [], f"{job} verifies only inside the clean runtime")
            for i in smoke:
                self.assertIsNone(wf.field(steps[i], "if"), "no smoke step may be skipped")
                self.assertIsNone(wf.field(steps[i], "continue-on-error"))

    def test_runtime_container_sees_only_the_archive_and_verifier_read_only(self):
        for name, job, row in self.each_job():
            _, body = self.runtime_body(name, job, row)
            artifact = row["artifact"]
            self.assertIn(row["runtime"], body)
            mounts = re.findall(r"-v \"([^\"]+)\"", body)
            self.assertEqual(sorted(mounts), sorted([
                f"$PWD/{artifact}.tar.gz:/release/{artifact}.tar.gz:ro",
                "$PWD/scripts/check-soapy-release.py:/release/check-soapy-release.py:ro",
            ]))
            for forbidden in ("--privileged", "--device", "-p ", "--volume", "$HOME", ".cargo",
                              "GITHUB_TOKEN", "--env", " -e "):
                self.assertNotIn(forbidden, body)

    def test_runtime_container_checks_the_missing_library_then_the_installed_one(self):
        for name, job, row in self.each_job():
            _, body = self.runtime_body(name, job, row)
            artifact = row["artifact"]
            missing = body.index(f"check-soapy-release.py --expect-missing-soapy-library /release/{artifact}.tar.gz")
            install = body.index("libsoapysdr0.8 ")
            guard = body.index("command -v cargo || command -v rustc")
            dev_guard = body.index("dpkg-query")
            final = body.index(f"check-soapy-release.py /release/{artifact}.tar.gz")
            self.assertLess(missing, install, "the missing-library check must run before the install")
            self.assertLess(install, guard)
            self.assertLess(guard, final)
            self.assertLess(dev_guard, final)
            installs = re.findall(r"apt-get install ([^\n]*)", body)
            self.assertEqual(len(installs), 2)
            packages = set()
            for line in installs:
                self.assertIn("--no-install-recommends", line)
                packages |= {w for w in line.split() if not w.startswith("-")}
            self.assertEqual(packages, {"python3", row["alsa"], "libsoapysdr0.8",
                                        "soapysdr0.8-module-rtlsdr", "soapysdr0.8-module-airspy",
                                        "soapysdr0.8-module-hackrf"})
            for word in ("cargo install", "rustup", "rustc ", "build-essential", "gcc"):
                self.assertNotIn(word, body.replace("command -v rustc", ""))

    def test_real_library_soapy_tests_run_with_the_release_features(self):
        for name in RELEASE_WORKFLOWS:
            with self.subTest(workflow=name, job="build-soapy"):
                bodies = [executable(wf.run_body(s)) for s in build_soapy_steps(name)]
                self.assertIn(SOAPY_TEST, [b.strip() for b in bodies])
            with self.subTest(workflow=name, job="build-soapy-arm64"):
                # MAN-539: inside the Debian bookworm build container, against bookworm's library.
                bodies = [executable(wf.run_body(s)) for s in build_soapy_steps(name, "build-soapy-arm64")]
                builds = [b for b in bodies if "rust:1-slim-bookworm" in b]
                self.assertEqual(len(builds), 1, "build-soapy-arm64 has one bookworm build container")
                self.assertIn(SOAPY_TEST, [line.strip() for line in builds[0].splitlines()])

    def test_verifier_runs_the_extracted_archive_not_the_build_tree(self):
        with open(VERIFIER, encoding="utf-8") as f:
            code = "\n".join(line for line in f.read().splitlines() if not line.lstrip().startswith("#"))
        self.assertNotIn("target/", code)
        self.assertNotIn("cargo", code.replace("Cargo", ""))
        self.assertIn("tempfile.TemporaryDirectory", code)


def is_build_push(step):
    return (wf.field(step, "uses") or "").startswith("docker/build-push-action@")


def with_field(step, key):
    block = wf.sub(step, "with")
    return wf.field(block, key) if block else None


class ImageCheckWiringTests(unittest.TestCase):
    """MAN-539: each Docker job builds and checks the arm64 image before the multi-platform push."""

    def test_each_docker_job_checks_the_arm64_image_before_the_push(self):
        for name in RELEASE_WORKFLOWS:
            for job in IMAGE_JOBS:
                with self.subTest(workflow=name, job=job):
                    steps = wf.steps(wf.job_block(wf.read(name), job))
                    self.assertTrue(steps, f"{name} has no {job} job")
                    smoke = index_of(steps, lambda s: is_build_push(s)
                                     and with_field(s, "platforms") == CHECK_PLATFORM, "arm64 image build", job)
                    check = index_of(steps, lambda s: executable(wf.run_body(s)).strip() == IMAGE_VERIFY,
                                     "image check", job)
                    push = index_of(steps, lambda s: is_build_push(s)
                                    and with_field(s, "platforms") == THREE_PLATFORMS, "multi-platform build", job)
                    self.assertLess(smoke, check)
                    self.assertLess(check, push, "the image must be checked before anything is pushed")
                    self.assertEqual(with_field(steps[smoke], "load"), "true")
                    self.assertEqual(with_field(steps[smoke], "tags"), CHECK_IMAGE)
                    self.assertIsNone(with_field(steps[smoke], "push"))
                    self.assertEqual(wf.field(steps[smoke], "uses"), wf.field(steps[push], "uses"),
                                     "the check build pins the same action as the push")
                    # MAN-537: the image reports the release commit, and identical build-args let
                    # the push reuse the check build's arm64 layers.
                    for i in (smoke, push):
                        self.assertEqual(with_field(steps[i], "build-args"), IMAGE_BUILD_ARGS)
                    for i in (smoke, check):
                        # A publish=false dispatch still checks the image.
                        self.assertIsNone(wf.field(steps[i], "if"))
                        self.assertIsNone(wf.field(steps[i], "continue-on-error"))
                    self.assertEqual(len([s for s in steps if is_build_push(s)]), 2)

    def test_image_jobs_still_do_not_wait_for_the_archive_jobs(self):
        # MAN-298, MAN-82 D5: the image jobs need only validate-tag and build.
        for name in RELEASE_WORKFLOWS:
            text = wf.read(name)
            for job in IMAGE_JOBS:
                with self.subTest(workflow=name, job=job):
                    self.assertEqual(wf.field(wf.job_block(text, job), "needs"), "[validate-tag, build]")


class CiWiringTests(unittest.TestCase):
    def test_runs_in_required_test_job_on_unix_legs(self):
        wanted = "python3 -m unittest scripts.tests.test_soapy_release -v"
        steps = wf.steps(wf.job_block(wf.read("ci-full.yml"), "test"))
        matches = [s for s in steps if wf.field(s, "run") == wanted]
        self.assertEqual(len(matches), 1, f"ci-full.yml job test has no step running {wanted!r}")
        self.assertEqual(wf.field(matches[0], "if"), "runner.os != 'Windows'")


if __name__ == "__main__":
    unittest.main()
