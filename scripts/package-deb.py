#!/usr/bin/env python3
"""Build one Debian package of a release binary (MAN-540).

Stages the built binary at /usr/bin/manta, the systemd unit from
packaging/systemd/manta.service with its one ExecStart path changed from
/usr/local/bin/manta to /usr/bin/manta, the `manta config init` scaffold at
/usr/share/manta/manta.toml, the man pages `manta man-pages --out` generates
(passed in via --man-dir), the DEP-5 copyright file and the maintainer
scripts in packaging/deb/, then runs `dpkg-deb --root-owner-group -Zxz
--build` to write `<out-dir>/<artifact>.deb`. The package has no conffiles:
the postinst copies the scaffold to /etc/manta/manta.toml once, when no file
is there, so no upgrade replaces or prompts about the operator's config
(docs/DECISIONS/2026-11-09-man540-debian-package.md).

The release workflow (.github/workflows/release-publish.yml) calls this from
the `build` job's two Linux legs, after the archive step and from the same
binary. Tested by scripts/tests/test_package_deb.py.

Release tooling only: this script is not itself shipped in the package.

Exit codes: 0 on success, 1 if an input is missing, the binary is not a
Linux ELF for the target, or dpkg-deb is missing or fails (no package is left
behind), 2 on a usage error.
"""
import argparse
import gzip
import hashlib
import os
import re
import shutil
import struct
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

TARGET_ARCH = {"x86_64-unknown-linux-gnu": "amd64", "aarch64-unknown-linux-gnu": "arm64"}
ELF_MACHINE = {"amd64": 62, "arm64": 183}
ELF_NAME = {62: "x86_64", 183: "aarch64"}

KIT_EXEC = "ExecStart=/usr/local/bin/manta "
PKG_EXEC = "ExecStart=/usr/bin/manta "

UNIT = "packaging/systemd/manta.service"
SCAFFOLD = "crates/manta-cli/src/config_init.toml"
SCRIPTS = ("postinst", "prerm", "postrm")
COPYRIGHT = "packaging/deb/copyright"

MAINTAINER = "Hagale Technologies, LLC <tony@hagale.net>"
DEPENDS = "libasound2 (>= 1.0.16) | libasound2t64"
# systemd 247 is the first that expands ${CREDENTIALS_DIRECTORY} in ExecStart.
BREAKS = "systemd (<< 247)"
HOMEPAGE = "https://github.com/HagaleTechnologies/manta"
DESCRIPTION = """wideband multi-signal CW skimmer
 manta decodes every CW signal across an SDR passband and emits
 RBN-compatible spots over telnet, JSON Lines and WebSocket.
 This build reads KiwiSDR, HPSDR, ka9q-radio, sound-card and WAV input.
 For SoapySDR receivers use the manta-linux-*-soapy release archives."""

# Copied from scripts/release-version.sh: SemVer 2.0.0 without build metadata.
SEMVER_RE = re.compile(
    r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(-((0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)"
    r"(\.(0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*))?$"
)
DISPATCH_RE = re.compile(r"^dispatch-([0-9]+)$")


class UsageError(Exception):
    pass


class InputError(Exception):
    pass


def debian_version(version):
    """The Debian version for a release version (validate-tag's output)."""
    m = DISPATCH_RE.match(version)
    if m:
        # A workflow_dispatch rehearsal sorts below every real release.
        return f"0~dispatch.{m.group(1)}-1"
    m = SEMVER_RE.match(version)
    if not m:
        raise UsageError(f"--version {version!r} is not X.Y.Z, X.Y.Z-pre or dispatch-<run id>")
    core = f"{m.group(1)}.{m.group(2)}.{m.group(3)}"
    if m.group(5):
        # `~` sorts a pre-release before its release, as SemVer does.
        return f"{core}~{m.group(5)}-1"
    return f"{core}-1"


def architecture(target):
    try:
        return TARGET_ARCH[target]
    except KeyError:
        raise UsageError(f"--target {target!r} has no Debian package; use one of "
                         + ", ".join(sorted(TARGET_ARCH))) from None


def check_elf(path, arch):
    """Refuse anything but a 64-bit little-endian ELF for `arch`."""
    with open(path, "rb") as f:
        head = f.read(20)
    if len(head) < 20 or head[:4] != b"\x7fELF" or head[4] != 2 or head[5] != 1:
        raise InputError(f"{path} is not a 64-bit little-endian ELF binary")
    (machine,) = struct.unpack_from("<H", head, 18)
    want = ELF_MACHINE[arch]
    if machine != want:
        got = ELF_NAME.get(machine, f"e_machine {machine}")
        raise InputError(f"{path} is a {got} binary; a {arch} package needs {ELF_NAME[want]}")


def packaged_unit(text):
    """The kit unit with its one ExecStart path moved to /usr/bin/manta."""
    lines = text.splitlines(keepends=True)
    hits = [i for i, line in enumerate(lines) if line.startswith(KIT_EXEC)]
    if len(hits) != 1:
        raise InputError(f"expected exactly one line starting {KIT_EXEC.strip()!r} in the unit, "
                         f"found {len(hits)} ExecStart lines like it")
    i = hits[0]
    lines[i] = PKG_EXEC + lines[i][len(KIT_EXEC):]
    return "".join(lines)


def require(path):
    if not os.path.isfile(path):
        raise InputError(f"package-deb: missing required input: {path}")
    return path


def man_pages(man_dir):
    if not os.path.isdir(man_dir):
        raise InputError(f"package-deb: missing required input: {man_dir}")
    pages = sorted(n for n in os.listdir(man_dir)
                   if n.endswith(".1") and os.path.isfile(os.path.join(man_dir, n)))
    if "manta.1" not in pages:
        raise InputError(f"package-deb: missing required input: {os.path.join(man_dir, 'manta.1')}")
    return pages


def copy_file(src, dest, mode):
    shutil.copyfile(src, dest)
    os.chmod(dest, mode)


def write_bytes(dest, data, mode):
    with open(dest, "wb") as f:
        f.write(data)
    os.chmod(dest, mode)


def stage(stage_dir, args, arch, debver):
    root = args.root
    unit = require(os.path.join(root, *UNIT.split("/")))
    scaffold = require(os.path.join(root, *SCAFFOLD.split("/")))
    copyright_file = require(os.path.join(root, *COPYRIGHT.split("/")))
    scripts = [require(os.path.join(root, "packaging", "deb", s)) for s in SCRIPTS]
    binary = require(args.binary)
    pages = man_pages(args.man_dir)
    check_elf(binary, arch)
    with open(unit, encoding="utf-8") as f:
        unit_text = packaged_unit(f.read())

    os.chmod(stage_dir, 0o755)
    for d in ("etc/manta", "usr/bin", "usr/lib/systemd/system", "usr/share/manta",
              "usr/share/man/man1", "usr/share/doc/manta", "DEBIAN"):
        parts = d.split("/")
        for n in range(1, len(parts) + 1):
            path = os.path.join(stage_dir, *parts[:n])
            os.makedirs(path, exist_ok=True)
            os.chmod(path, 0o755)

    def at(rel):
        return os.path.join(stage_dir, *rel.split("/"))

    data = []  # (relpath) of every regular data file, for md5sums and Installed-Size
    copy_file(binary, at("usr/bin/manta"), 0o755)
    data.append("usr/bin/manta")
    write_bytes(at("usr/lib/systemd/system/manta.service"), unit_text.encode("utf-8"), 0o644)
    data.append("usr/lib/systemd/system/manta.service")
    copy_file(scaffold, at("usr/share/manta/manta.toml"), 0o644)
    data.append("usr/share/manta/manta.toml")
    copy_file(copyright_file, at("usr/share/doc/manta/copyright"), 0o644)
    data.append("usr/share/doc/manta/copyright")
    for page in pages:
        rel = f"usr/share/man/man1/{page}.gz"
        with open(os.path.join(args.man_dir, page), "rb") as src, open(at(rel), "wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as gz:
                shutil.copyfileobj(src, gz)
        os.chmod(at(rel), 0o644)
        data.append(rel)

    for src in scripts:
        copy_file(src, at("DEBIAN/" + os.path.basename(src)), 0o755)

    sums = []
    size_kib = 0
    for rel in sorted(data):
        with open(at(rel), "rb") as f:
            body = f.read()
        sums.append(f"{hashlib.md5(body).hexdigest()}  {rel}\n")
        size_kib += -(-len(body) // 1024)
    write_bytes(at("DEBIAN/md5sums"), "".join(sums).encode("utf-8"), 0o644)

    control = (
        f"Package: manta\n"
        f"Version: {debver}\n"
        f"Architecture: {arch}\n"
        f"Maintainer: {MAINTAINER}\n"
        f"Installed-Size: {size_kib}\n"
        f"Depends: {DEPENDS}\n"
        f"Breaks: {BREAKS}\n"
        f"Section: hamradio\n"
        f"Priority: optional\n"
        f"Homepage: {HOMEPAGE}\n"
        f"Description: {DESCRIPTION}\n"
    )
    write_bytes(at("DEBIAN/control"), control.encode("utf-8"), 0o644)


def build(args):
    arch = architecture(args.target)
    debver = debian_version(args.version)
    if "/" in args.artifact or args.artifact in ("", ".", ".."):
        raise UsageError(f"--artifact {args.artifact!r} must be one path component")
    dpkg_deb = shutil.which("dpkg-deb")
    if not dpkg_deb:
        raise InputError("package-deb: dpkg-deb not found; build Debian packages on Debian or Ubuntu")
    os.makedirs(args.out_dir, exist_ok=True)
    final = os.path.join(args.out_dir, f"{args.artifact}.deb")
    partial = os.path.join(args.out_dir, f".{args.artifact}.deb.partial")
    with tempfile.TemporaryDirectory(prefix="package-deb-") as tmp:
        stage_dir = os.path.join(tmp, "manta")
        os.mkdir(stage_dir)
        stage(stage_dir, args, arch, debver)
        try:
            proc = subprocess.run(
                [dpkg_deb, "--root-owner-group", "-Zxz", "--build", stage_dir, partial],
                stdin=subprocess.DEVNULL, capture_output=True, text=True)
            if proc.returncode != 0:
                raise InputError(f"package-deb: dpkg-deb failed ({proc.returncode}): "
                                 f"{(proc.stderr or proc.stdout).strip()}")
            os.replace(partial, final)
        finally:
            if os.path.exists(partial):
                os.remove(partial)
    print(f"package-deb: wrote {final} (manta {debver} {arch})")
    return 0


def parse(argv):
    p = argparse.ArgumentParser(prog="package-deb.py", description=__doc__.splitlines()[0])
    p.add_argument("--binary", required=True, help="the built manta binary")
    p.add_argument("--target", required=True, help="the Rust target triple it was built for")
    p.add_argument("--version", required=True, help="validate-tag's version output")
    p.add_argument("--artifact", required=True, help="output file name without .deb")
    p.add_argument("--man-dir", required=True, help="the `manta man-pages --out` directory")
    p.add_argument("--out-dir", required=True, help="where <artifact>.deb is written")
    p.add_argument("--root", default=ROOT, help="the repository root (default: this checkout)")
    return p.parse_args(argv)


def main(argv=None):
    args = parse(argv)  # argparse exits 2 on a usage error
    try:
        return build(args)
    except UsageError as e:
        print(f"package-deb: {e}", file=sys.stderr)
        return 2
    except InputError as e:
        msg = str(e)
        print(msg if msg.startswith("package-deb:") else f"package-deb: {msg}", file=sys.stderr)
        return 1
    except OSError as e:
        print(f"package-deb: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
