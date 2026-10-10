#!/usr/bin/env python3
"""Build one native release archive (MAN-268).

Copies the built binary, README.md, both licence files and the unattended-
service kit into a fresh scratch staging tree, then writes
`<out-dir>/<artifact>.tar.gz` or `<out-dir>/<artifact>.zip` containing exactly
one top-level `<artifact>/` directory. Both release workflows
(.github/workflows/release.yml, release-publish.yml) call this from one
`shell: bash` step on every runner, so tar.gz and zip get the same files.
Tested by scripts/tests/test_package_release.py.

Release tooling only: this script is not itself shipped in the archive.

Exit codes: 0 on success, 1 if an input is missing or the archive cannot be
written (no archive is left behind), 2 on a usage error.
"""
import argparse
import gzip
import os
import shutil
import stat
import sys
import tarfile
import tempfile
import time
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Repo-relative files copied into the archive at the same relative paths. The
# old Unix step tolerated missing licences (`2>/dev/null ||`); both are
# tracked, so a missing one now fails the release instead of shipping without.
ASSETS = (
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

FORMATS = {"tar.gz": ".tar.gz", "zip": ".zip"}


def parse_args(argv):
    p = argparse.ArgumentParser(description="Build one manta release archive.")
    p.add_argument("--binary", required=True, help="the built manta binary")
    p.add_argument("--artifact", required=True,
                   help="top-level directory and archive basename, e.g. manta-linux-x86_64")
    p.add_argument("--format", required=True, choices=sorted(FORMATS))
    p.add_argument("--out-dir", required=True, help="directory the archive is written to")
    p.add_argument("--root", default=ROOT, help="repository root holding the assets (default: this checkout)")
    args = p.parse_args(argv)
    name = args.artifact
    if name in ("", ".", "..") or "/" in name or "\\" in name:
        p.error(f"--artifact must be a single path component, got {name!r}")
    return args


def missing_inputs(binary, root):
    paths = [binary] + [os.path.join(root, *rel.split("/")) for rel in ASSETS]
    return [path for path in paths if not os.path.isfile(path)]


def stage(binary, root, staging):
    """Copy the binary and every asset under `staging`; return {relative path: mode}."""
    os.makedirs(staging)
    bin_name = os.path.basename(binary)
    shutil.copyfile(binary, os.path.join(staging, bin_name))
    modes = {bin_name: 0o755}
    for rel in ASSETS:
        dest = os.path.join(staging, *rel.split("/"))
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        shutil.copyfile(os.path.join(root, *rel.split("/")), dest)
        modes[rel] = 0o755 if rel.endswith(".sh") else 0o644
    return modes


def entries(staging, artifact, modes):
    """(archive name, source path, mode) for every directory and file under
    `staging`, sorted, parents before children. Modes come from `modes`, not
    the staging filesystem, so a Windows runner records the same bits."""
    out = [(artifact, staging, None)]
    for dirpath, dirnames, filenames in os.walk(staging):
        dirnames.sort()
        rel_dir = os.path.relpath(dirpath, staging).replace(os.sep, "/")
        for name in sorted(dirnames) + sorted(filenames):
            rel = name if rel_dir == "." else f"{rel_dir}/{name}"
            out.append((f"{artifact}/{rel}", os.path.join(dirpath, name), modes.get(rel)))
    return sorted(out)


def write_tar(path, items, mtime, artifact):
    # The gzip header names `<artifact>.tar`, not the temporary file.
    with open(path, "wb") as raw, \
            gzip.GzipFile(filename=artifact + ".tar", mode="wb", fileobj=raw, mtime=mtime) as gz, \
            tarfile.open(fileobj=gz, mode="w") as tar:
        for arcname, source, mode in items:
            info = tar.gettarinfo(source, arcname)
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = mtime
            if info.isdir():
                info.mode = 0o755
                tar.addfile(info)
            else:
                info.mode = mode
                with open(source, "rb") as f:
                    tar.addfile(info, f)


def write_zip(path, items, mtime):
    stamp = time.gmtime(max(mtime, 315532800))[:6]  # zip cannot store times before 1980
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as zf:
        for arcname, source, mode in items:
            if os.path.isdir(source):
                info = zipfile.ZipInfo(arcname + "/", stamp)
                info.external_attr = ((stat.S_IFDIR | 0o755) << 16) | 0x10
                data = b""
            else:
                info = zipfile.ZipInfo(arcname, stamp)
                info.external_attr = (stat.S_IFREG | mode) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                with open(source, "rb") as f:
                    data = f.read()
            info.create_system = 3  # Unix, so unzip honours the mode bits
            zf.writestr(info, data)


def main(argv=None):
    args = parse_args(argv)
    missing = missing_inputs(args.binary, args.root)
    if missing:
        for path in missing:
            print(f"package-release: missing required input: {path}", file=sys.stderr)
        print("package-release: no archive written", file=sys.stderr)
        return 1

    os.makedirs(args.out_dir, exist_ok=True)
    final = os.path.join(args.out_dir, args.artifact + FORMATS[args.format])
    mtime = int(time.time())
    with tempfile.TemporaryDirectory(prefix="manta-package-") as scratch:
        staging = os.path.join(scratch, args.artifact)
        modes = stage(args.binary, args.root, staging)
        items = entries(staging, args.artifact, modes)
        # Build beside the destination, then rename: a failure never leaves a
        # partial archive under the final name.
        fd, partial = tempfile.mkstemp(prefix=f".{args.artifact}.", suffix=".partial", dir=args.out_dir)
        os.close(fd)
        umask = os.umask(0)
        os.umask(umask)
        try:
            os.chmod(partial, 0o666 & ~umask)  # mkstemp's 0600 -> what a plain create gives
            if args.format == "tar.gz":
                write_tar(partial, items, mtime, args.artifact)
            else:
                write_zip(partial, items, mtime)
            os.replace(partial, final)
        except BaseException:
            os.unlink(partial)
            raise
    print(f"wrote {final}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
