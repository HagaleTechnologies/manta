#!/usr/bin/env python3
"""Check a built SoapySDR release archive (manta-linux-x86_64-soapy.tar.gz or manta-linux-arm64-soapy.tar.gz) the way an operator would run it (MAN-82, MAN-539).

Extracts ARCHIVE into a private temporary directory and runs only the extracted
`manta`, never a build tree's binary. The archive's root directory is named
after the file, so one check serves either architecture:

- `--version` succeeds and lists the `soapy` feature;
- `run`, `listen` and `check --help` succeed and offer every Soapy flag, and
  `--hpsdr-host` too;
- the archive's own manta-run.1 and manta-check.1 document the Soapy flags;
- `check --soapy-driver driver=man82_nonexistent ...` reaches the SoapySDR
  library and fails with its no-match error (exit 1), not with an argument
  error (exit 2) or a loader error (exit 127);
- `devices --json` prints JSON whose Soapy section enumerated without error.

With --expect-missing-soapy-library it instead checks that `--version` cannot
start because libSoapySDR.so.0.8 is absent: the clean-runtime release step runs
that before installing the library, so the missing-library failure operators
see is the one README documents.

With --docker-image IMAGE --platform PLATFORM (MAN-539) it checks a built
Docker image instead of an archive, running each command as
`docker run --rm --platform PLATFORM IMAGE <args>`. It makes the version,
help, nonexistent-driver and devices checks above (the image ships no man
pages), and one more: `check --soapy-driver driver=<d>` for each of the
rtlsdr, airspy and hackrf modules the image installs must fail with that
module's own open error (exit 1, `cannot open source`), never SoapySDR's
`no match`, which is what a missing module looks like. The release workflow's
Docker jobs run it on the arm64 image before the multi-platform push.

Release tooling only: the release workflows run it after packaging and before
upload, and this script is not itself shipped in the archive. Linux only; the
archive's binary is a Linux executable. Tested by
scripts/tests/test_soapy_release.py.

Exit codes: 0 when every check passes, 1 when a check fails, 2 on a usage error.
"""
import argparse
import json
import os
import re
import stat
import subprocess
import sys
import tarfile
import tempfile

ARCHIVE_SUFFIX = ".tar.gz"
# A manual set every release has; not the full list, so a new command needs no edit here.
REQUIRED_PAGES = ("manta.1", "manta-run.1", "manta-check.1", "manta-devices.1")
SOAPY_FLAGS = ("--soapy-driver", "--soapy-freq-hz", "--soapy-rate-hz", "--soapy-gain")
HELP_COMMANDS = ("run", "listen", "check")
SOAPY_PAGES = ("manta-run.1", "manta-check.1")
REQUIRED_FEATURES = {"soapy"}
MISSING_DRIVER = ("check", "--soapy-driver", "driver=man82_nonexistent",
                  "--soapy-freq-hz", "14025000", "--soapy-rate-hz", "96000")
NO_MATCH = "SoapySDR::Device::make() no match"
# MAN-539: the device modules the Docker image installs. With no device attached each must load
# and fail with its own error; a missing module makes SoapySDR answer NO_MATCH instead.
MODULE_DRIVERS = ("rtlsdr", "airspy", "hackrf")
MODULE_ARGS = ("--soapy-freq-hz", "14025000", "--soapy-rate-hz", "1024000")
OPEN_FAILED = "cannot open source"
MISSING_LIBRARY = "libSoapySDR.so.0.8: cannot open shared object file"
LOADER_FAILURE = 127
DEFAULT_TIMEOUT = 60.0


class CheckFailed(Exception):
    pass


def safe_extract(archive, dest):
    """Extract ARCHIVE under dest and return the path of its one top-level directory.

    The packager writes only regular files and directories under one root named
    after the archive, so anything else -- an absolute path, `..`, a link, a
    device -- is refused before a single byte is written."""
    name = os.path.basename(archive)
    if not name.endswith(ARCHIVE_SUFFIX):
        raise CheckFailed(f"{archive}: not a {ARCHIVE_SUFFIX} archive")
    root = name[:-len(ARCHIVE_SUFFIX)]
    try:
        with tarfile.open(archive, "r:gz") as tar:
            members = tar.getmembers()
            for member in members:
                parts = member.name.split("/")
                if member.name.startswith("/") or ".." in parts or parts[0] != root:
                    raise CheckFailed(f"{archive}: member {member.name!r} is outside {root}/")
                if not (member.isfile() or member.isdir()):
                    raise CheckFailed(f"{archive}: member {member.name!r} is not a regular file or directory")
            if hasattr(tarfile, "data_filter"):
                tar.extractall(dest, members=members, filter="data")
            else:
                tar.extractall(dest, members=members)
    except (OSError, tarfile.TarError) as e:
        raise CheckFailed(f"{archive}: cannot extract: {e}") from e
    top = os.path.join(dest, root)
    if not os.path.isdir(top):
        raise CheckFailed(f"{archive}: no {root}/ directory")
    return top


def run(prefix, args, timeout):
    """Run `manta ARGS` through PREFIX, the argv that starts manta: the extracted binary's path
    in archive mode, `docker run --rm --platform P IMAGE` in image mode."""
    try:
        return subprocess.run([*prefix, *args], capture_output=True, text=True,
                              errors="replace", timeout=timeout)
    except subprocess.TimeoutExpired as e:
        raise CheckFailed(f"`manta {' '.join(args)}` did not finish within {timeout:g} s") from e
    except OSError as e:
        raise CheckFailed(f"`manta {' '.join(args)}` could not start: {e}") from e


def describe(proc):
    return f"exit {proc.returncode}\nstdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"


def find_binary(top):
    binary = os.path.join(top, "manta")
    if not os.path.isfile(binary):
        raise CheckFailed(f"the archive has no {os.path.basename(top)}/manta")
    if not os.stat(binary).st_mode & stat.S_IXUSR:
        raise CheckFailed(f"{os.path.basename(top)}/manta is not executable")
    return binary


def image_prefix(image, platform):
    return ["docker", "run", "--rm", "--platform", platform, image]


def check_missing_library(binary, timeout):
    proc = run(binary, ["--version"], timeout)
    if proc.returncode != LOADER_FAILURE or MISSING_LIBRARY not in proc.stderr:
        raise CheckFailed("expected the loader to refuse to start manta for want of "
                          f"libSoapySDR.so.0.8 (exit {LOADER_FAILURE}), got {describe(proc)}")
    return [f"--version without libSoapySDR: exit {LOADER_FAILURE}, {MISSING_LIBRARY}"]


def check_pages(top):
    man = os.path.join(top, "man", "man1")
    for page in REQUIRED_PAGES:
        if not os.path.isfile(os.path.join(man, page)):
            raise CheckFailed(f"the archive has no man/man1/{page}")
    for page in SOAPY_PAGES:
        with open(os.path.join(man, page), encoding="utf-8", errors="replace") as f:
            text = f.read().replace("\\-", "-")  # roff writes a literal hyphen as \-
        for flag in SOAPY_FLAGS:
            if flag not in text:
                raise CheckFailed(f"man/man1/{page} does not document {flag}: "
                                  "the pages were not rendered from this binary")


def check_version(binary, timeout):
    proc = run(binary, ["--version"], timeout)
    if proc.returncode != 0:
        raise CheckFailed(f"`manta --version` failed: {describe(proc)}")
    m = re.search(r"features: ([^)\s]*)\)", proc.stdout)
    features = set(m.group(1).split(",")) if m else set()
    if not REQUIRED_FEATURES <= features:
        raise CheckFailed(f"`manta --version` does not list features {sorted(REQUIRED_FEATURES)}: "
                          f"{proc.stdout.strip()!r}")
    return proc.stdout.strip()


def check_help(binary, timeout):
    for command in HELP_COMMANDS:
        proc = run(binary, [command, "--help"], timeout)
        if proc.returncode != 0:
            raise CheckFailed(f"`manta {command} --help` failed: {describe(proc)}")
        for flag in SOAPY_FLAGS + ("--hpsdr-host",):
            if not re.search(re.escape(flag) + r"(?![\w-])", proc.stdout):
                raise CheckFailed(f"`manta {command} --help` does not offer {flag}")


def check_missing_driver(binary, timeout):
    proc = run(binary, list(MISSING_DRIVER), timeout)
    if proc.returncode != 1 or NO_MATCH not in proc.stderr:
        raise CheckFailed("expected a nonexistent Soapy driver to reach the SoapySDR library and "
                          f"fail with {NO_MATCH!r} (exit 1), got {describe(proc)}")


def check_devices(binary, timeout):
    # Audio enumeration may fail on a host without sound devices (exit 1); that
    # says nothing about Soapy, so only a crash or bad JSON fails here.
    proc = run(binary, ["devices", "--json"], timeout)
    if proc.returncode not in (0, 1):
        raise CheckFailed(f"`manta devices --json` failed: {describe(proc)}")
    try:
        report = json.loads(proc.stdout)
        soapy = report["soapy"]
    except (ValueError, KeyError, TypeError) as e:
        raise CheckFailed(f"`manta devices --json` printed no Soapy section: {describe(proc)}") from e
    if not isinstance(soapy, dict) or soapy.get("status") != "ok" or soapy.get("error") is not None:
        raise CheckFailed(f"`manta devices --json` Soapy enumeration did not succeed: {soapy!r}")
    return len(soapy.get("devices") or [])


def check_modules(binary, timeout):
    for driver in MODULE_DRIVERS:
        proc = run(binary, ["check", "--soapy-driver", f"driver={driver}", *MODULE_ARGS], timeout)
        if proc.returncode != 1 or OPEN_FAILED not in proc.stderr or NO_MATCH in proc.stderr:
            raise CheckFailed(f"expected the {driver} SoapySDR module to load and fail with its own "
                              f"open error ({OPEN_FAILED!r}, exit 1, not {NO_MATCH!r}), "
                              f"got {describe(proc)}")


def verify(archive, expect_missing_library=False, timeout=DEFAULT_TIMEOUT):
    """Run every check against ARCHIVE; return report lines, or raise CheckFailed."""
    with tempfile.TemporaryDirectory(prefix="manta-soapy-check-") as tmp:
        top = safe_extract(archive, tmp)
        binary = [find_binary(top)]
        check_pages(top)
        if expect_missing_library:
            return check_missing_library(binary, timeout)
        version = check_version(binary, timeout)
        check_help(binary, timeout)
        check_missing_driver(binary, timeout)
        devices = check_devices(binary, timeout)
        return [version,
                f"run/listen/check --help offer {', '.join(SOAPY_FLAGS)} and --hpsdr-host",
                f"man/man1/{' and '.join(SOAPY_PAGES)} document the Soapy flags",
                f"nonexistent driver: exit 1, {NO_MATCH}",
                f"devices --json: Soapy enumeration ok, {devices} device(s)"]


def verify_image(image, platform, timeout=DEFAULT_TIMEOUT):
    """Run the image checks against IMAGE on PLATFORM; return report lines, or raise CheckFailed."""
    prefix = image_prefix(image, platform)
    version = check_version(prefix, timeout)
    check_help(prefix, timeout)
    check_missing_driver(prefix, timeout)
    devices = check_devices(prefix, timeout)
    check_modules(prefix, timeout)
    return [version,
            f"run/listen/check --help offer {', '.join(SOAPY_FLAGS)} and --hpsdr-host",
            f"nonexistent driver: exit 1, {NO_MATCH}",
            f"devices --json: Soapy enumeration ok, {devices} device(s)",
            f"modules {', '.join(MODULE_DRIVERS)} load: each driver fails with its own error, not no match"]


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("archive", nargs="?",
                        help="path to a manta-linux-<arch>-soapy.tar.gz")
    parser.add_argument("--expect-missing-soapy-library", action="store_true",
                        help="check that the binary cannot start without libSoapySDR.so.0.8 (archive only)")
    parser.add_argument("--docker-image", metavar="IMAGE",
                        help="check this local Docker image instead of an archive (needs --platform)")
    parser.add_argument("--platform", help="the --docker-image platform to run, e.g. linux/arm64")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT,
                        help=f"seconds allowed for each manta invocation (default {DEFAULT_TIMEOUT:g})")
    args = parser.parse_args(argv)
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    if (args.archive is None) == (args.docker_image is None):
        parser.error("give exactly one of ARCHIVE or --docker-image")
    if args.docker_image is not None and not args.platform:
        parser.error("--docker-image needs --platform")
    if args.docker_image is None and args.platform is not None:
        parser.error("--platform goes with --docker-image")
    if args.docker_image is not None and args.expect_missing_soapy_library:
        parser.error("--expect-missing-soapy-library checks an archive, not --docker-image")
    try:
        if args.docker_image is not None:
            lines = verify_image(args.docker_image, args.platform, args.timeout)
        else:
            lines = verify(args.archive, args.expect_missing_soapy_library, args.timeout)
    except CheckFailed as e:
        print(f"check-soapy-release: FAIL: {e}", file=sys.stderr)
        return 1
    for line in lines:
        print(f"OK: {line}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
