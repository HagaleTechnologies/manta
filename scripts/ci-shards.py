#!/usr/bin/env python3
"""Run complete Cargo test executables, preserving libtest's process-local locks.

Doctests are deliberately a separate required `cargo test --workspace --doc --locked`.
Timing weights affect placement only. Every discovered executable is always assigned.

Three modes (MAN-518):
- default: build the workspace's test executables, list them all, run this shard's group;
- `--build-bundles DIR`: build once, list once, assign every group and write one
  `slice-<i>.tar.gz` per shard holding its executables and a `shard-plan.json`;
- `--from-bundle FILE`: check the slice's plan against this checkout, extract it, re-list
  and run its group. This mode never invokes `cargo`, `rustc` or `rustup`; children see
  tripwire shims for all three.
"""
import argparse
import collections
import concurrent.futures
import io
import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
import tarfile
import tempfile
import time

PLAN_NAME = 'shard-plan.json'
INVENTORY_FIELDS = ('key', 'package', 'kind', 'name', 'executable', 'tests', 'ignored')
TRIPWIRE_TOOLS = ('cargo', 'rustc', 'rustup')
TRIPWIRE_EXIT = 97


def parse_artifacts(raw):
    found = {}
    for line in raw.splitlines():
        msg = json.loads(line)
        if msg.get('reason') != 'compiler-artifact' or not msg['profile']['test'] or not msg.get('executable'):
            continue
        target = msg['target']
        kind = target['kind']
        if len(kind) != 1 or kind[0] not in ('lib', 'bin', 'test', 'example', 'proc-macro'):
            raise ValueError(f'unknown test executable category: {kind}')
        key = f"{msg['package_id']}::{kind[0]}::{target['name']}"
        item = dict(key=key, package=msg['package_id'], kind=kind[0], name=target['name'], executable=msg['executable'])
        if key in found and found[key] != item:
            raise ValueError(f'ambiguous artifact: {key}')
        found[key] = item
    return sorted(found.values(), key=lambda x: x['key'])


def parse_bins(raw):
    """Non-test `bin` executables per package: what `CARGO_BIN_EXE_<name>` points at."""
    found = collections.defaultdict(set)
    for line in raw.splitlines():
        msg = json.loads(line)
        if msg.get('reason') != 'compiler-artifact' or msg['profile']['test'] or not msg.get('executable'):
            continue
        if msg['target']['kind'] == ['bin']:
            found[msg['package_id']].add(msg['executable'])
    return {package: sorted(paths) for package, paths in found.items()}


def assign(items, weights, count):
    if count < 2:
        raise ValueError('at least two shards are required')
    shards, loads = [[] for _ in range(count)], [0] * count
    for item in sorted(items, key=lambda x: (-weights.get(x['name'], 30), x['key'])):
        weight = weights.get(item['name'], 30)
        index = min(range(count), key=lambda i: (loads[i], i))
        shards[index].append(item)
        loads[index] += weight
    assert collections.Counter(x['key'] for group in shards for x in group) == collections.Counter(x['key'] for x in items)
    return shards


def cargo_env(package, target_dir, rustlib, cargo):
    """Cargo's per-test variables and dynamic-library path entries, as plain (JSON) data."""
    variables = dict(CARGO_MANIFEST_DIR=str(Path(package['manifest_path']).parent),
                     CARGO_MANIFEST_PATH=package['manifest_path'], CARGO_PKG_NAME=package['name'],
                     CARGO_PKG_VERSION=package['version'], CARGO=cargo)
    for variable, field in {
        'AUTHORS': 'authors', 'DESCRIPTION': 'description', 'HOMEPAGE': 'homepage',
        'LICENSE': 'license', 'LICENSE_FILE': 'license_file', 'README': 'readme',
        'REPOSITORY': 'repository', 'RUST_VERSION': 'rust_version',
    }.items():
        value = package.get(field)
        variables[f'CARGO_PKG_{variable}'] = ':'.join(value) if isinstance(value, list) else str(value or '')
    version = package['version'].split('+', 1)[0]
    release, _, pre = version.partition('-')
    major, minor, patch = release.split('.')
    variables.update(CARGO_PKG_VERSION_MAJOR=major, CARGO_PKG_VERSION_MINOR=minor,
                     CARGO_PKG_VERSION_PATCH=patch, CARGO_PKG_VERSION_PRE=pre)
    # Cargo includes build-script OUT_DIR entries on the dynamic-library path. Include all
    # native outputs from this build, as well as deps and the toolchain's Rust libraries.
    paths = [str(p) for p in (Path(target_dir)/'debug/build').glob('*/out')]
    paths += [str(Path(target_dir)/'debug/deps'), str(Path(target_dir)/'debug'), rustlib, str(Path(rustlib).parents[2])]
    return variables, paths


def compose_env(base, variables, paths):
    env = dict(base)
    env.update(variables)
    name = 'DYLD_FALLBACK_LIBRARY_PATH' if sys.platform == 'darwin' else 'LD_LIBRARY_PATH'
    env[name] = os.pathsep.join(list(paths) + ([base[name]] if base.get(name) else []))
    return env


def child_env(package, target_dir, rustlib):
    return compose_env(os.environ, *cargo_env(package, target_dir, rustlib, shutil.which('cargo') or 'cargo'))


def listing(item, cwd, env, ignored=False):
    command = [item['executable'], '--list', '--format', 'terse'] + (['--ignored'] if ignored else [])
    proc = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True, check=True)
    names = []
    for line in proc.stdout.splitlines():
        if not line:
            continue
        if not line.endswith((': test', ': benchmark')):
            raise ValueError(f"unsupported listing for {item['key']}: {line}")
        names.append(line.rsplit(': ', 1)[0])
    return sorted(names)


def list_item(item, cwd, env, expected=None):
    """List an executable's tests and ignored tests; with `expected`, require the same lists."""
    tests, ignored = listing(item, cwd, env), listing(item, cwd, env, True)
    if expected is not None and (tests, ignored) != (expected['tests'], expected['ignored']):
        raise ValueError(f"listing drift for {item['key']}")
    return tests, ignored


def inventory_view(item):
    return {field: item[field] for field in INVENTORY_FIELDS}


def git_identity(cwd, required):
    """`HEAD` and its tree; required in both bundle modes, best effort in the default mode."""
    try:
        head, tree = (subprocess.check_output(['git', 'rev-parse', rev], cwd=cwd, text=True, stderr=subprocess.PIPE).strip()
                      for rev in ('HEAD', 'HEAD^{tree}'))
        return head, tree
    except (OSError, subprocess.CalledProcessError) as error:
        if required:
            raise RuntimeError(f'cannot read the git commit: {error}') from error
        return None, None


def audit_evidence(records, support, workflow_started, completed, reference=None, build=None):
    if not records:
        raise ValueError('missing shards')
    count = records[0]['count']
    if sorted(r['index'] for r in records) != list(range(count)) or any(r['count'] != count for r in records):
        raise ValueError('missing or duplicate shard')
    def canonical(items):
        return sorted((x['key'], x['tests'], x['ignored']) for x in items)

    inventory = records[0]['inventory']
    if any(canonical(r['inventory']) != canonical(inventory) for r in records):
        raise ValueError('inventory or ignored-set mismatch')
    if reference is not None and canonical(reference) != canonical(inventory):
        raise ValueError('unsharded reference inventory mismatch')
    if any('gitHead' in r for r in records) and len({r.get('gitHead') for r in records}) != 1:
        raise ValueError('shards tested different commits')
    if build is not None:
        if canonical(build['inventory']) != canonical(inventory) or build.get('gitHead') != records[0].get('gitHead'):
            raise ValueError('build record disagrees with the shards')
    expected = collections.Counter(x['key'] for x in inventory)
    selected = collections.Counter(x['key'] for r in records for x in r['selected'])
    results = [x for r in records for x in r['results']]
    if selected != expected or collections.Counter(x['key'] for x in results) != expected or any(x['exitCode'] != 0 for x in results):
        raise ValueError('missing, duplicate or failed binary')
    if support.get('conclusion') != 'success' or support.get('doctests') != 'success':
        raise ValueError('missing or failed support/doctests')
    if not workflow_started <= support['started'] <= support['completed'] <= completed:
        raise ValueError('invalid workflow interval')
    return completed - workflow_started


def write_slice(path, plan, root, files):
    """`shard-plan.json` first, then each file under its workspace-relative name (links followed)."""
    data = json.dumps(plan, indent=2).encode()
    info = tarfile.TarInfo(PLAN_NAME)
    info.size, info.mode, info.mtime = len(data), 0o644, int(time.time())
    raw = 0
    with tarfile.open(path, 'w:gz', compresslevel=1, dereference=True) as tar:
        tar.addfile(info, io.BytesIO(data))
        for name in files:
            # Resolve the directory only: `target/debug/<bin>` may itself be an uplifted link.
            source = Path(os.path.realpath(Path(name).parent))/Path(name).name
            tar.add(str(source), arcname=source.relative_to(root).as_posix(), recursive=False)
            raw += source.stat().st_size
    return dict(index=plan['index'], files=len(files), rawBytes=raw, bytes=Path(path).stat().st_size)


def read_plan(tar):
    member = tar.next()
    if member is None or member.name != PLAN_NAME or not member.isfile():
        raise ValueError(f'bundle does not start with {PLAN_NAME}')
    return json.loads(tar.extractfile(member).read())


def safe_members(tar, target_rel):
    """Every member but the plan must be a regular file under the target directory."""
    target = Path(target_rel).as_posix().rstrip('/') + '/'
    members = []
    for member in tar.getmembers():
        if member.name == PLAN_NAME:
            continue
        name = member.name
        parts = Path(name).parts
        if name.startswith('/') or '..' in parts or not member.isreg() or not name.startswith(target):
            raise ValueError(f'unsafe bundle member: {name}')
        members.append(member)
    return members


def extract_members(tar, members, root):
    if hasattr(tarfile, 'data_filter'):
        tar.extractall(str(root), members=members, filter='data')
    else:
        tar.extractall(str(root), members=members)


def make_tripwire(directory):
    directory = Path(directory)
    for tool in TRIPWIRE_TOOLS:
        shim = directory/tool
        shim.write_text(f'#!/bin/sh\necho "MAN-518 tripwire: a shard invoked {tool}" >&2\nexit {TRIPWIRE_EXIT}\n')
        shim.chmod(0o755)
    return directory


def run_env(item, tripwire):
    """The child environment of a prebuilt binary: the build's Cargo variables, no compiler."""
    variables = dict(item['env'])
    variables['CARGO'] = str(Path(tripwire)/'cargo')
    env = compose_env(os.environ, variables, item['libraryPath'])
    env['PATH'] = os.pathsep.join([str(tripwire)] + ([env['PATH']] if env.get('PATH') else []))
    return env


def run_item(item, cwd, env):
    command = [item['executable']]
    print(f"Running {item['key']} from {cwd}", flush=True)
    started = time.monotonic()
    result = subprocess.run(command, cwd=cwd, env=env)
    return dict(key=item['key'], command=command, cwd=str(cwd), seconds=time.monotonic()-started, exitCode=result.returncode)


def cargo_build(record):
    started = time.monotonic()
    proc = subprocess.run(['cargo', 'test', '--workspace', '--locked', '--no-run', '--message-format=json'], text=True, stdout=subprocess.PIPE)
    record['buildSeconds'] = time.monotonic() - started
    if proc.returncode:
        raise RuntimeError(f'Cargo build exited {proc.returncode}')
    return proc.stdout


def load_weights():
    return json.loads(Path(__file__).with_name('ci-test-weights.json').read_text())['weights']


def build_bundles(args, record):
    record.pop('index')  # a build record is no shard's evidence
    meta = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version=1']))
    root = Path(meta['workspace_root']).resolve()
    target_dir = Path(meta['target_directory']).resolve()
    if root not in target_dir.parents:
        raise ValueError(f'target directory {target_dir} is not inside the workspace {root}')
    record['gitHead'], record['gitTree'] = git_identity(root, required=True)
    raw = cargo_build(record)
    items = parse_artifacts(raw)
    if not items:
        raise ValueError('empty workspace inventory')
    bins = parse_bins(raw)
    packages = {p['id']: p for p in meta['packages']}
    sysroot = subprocess.check_output(['rustc', '--print', 'target-libdir'], text=True).strip()
    cargo = shutil.which('cargo') or 'cargo'
    for item in items:
        package = packages[item['package']]
        variables, paths = cargo_env(package, target_dir, sysroot, cargo)
        cwd = Path(package['manifest_path']).parent
        item['tests'], item['ignored'] = list_item(item, cwd, compose_env(os.environ, variables, paths))
        item.update(cwd=cwd.resolve().relative_to(root).as_posix(), env=variables, libraryPath=paths)
    groups = assign(items, load_weights(), args.count)
    inventory = [inventory_view(x) for x in items]
    record.update(role='build', inventory=inventory, groups=[[x['key'] for x in group] for group in groups])
    args.build_bundles.mkdir(parents=True, exist_ok=True)

    def pack(index):
        group = groups[index]
        files = {x['executable'] for x in group}
        for package in {x['package'] for x in group}:
            files.update(bins.get(package, []))
        plan = dict(version=1, index=index, count=args.count, workspaceRoot=str(root), targetDirectory=str(target_dir),
                    gitHead=record['gitHead'], gitTree=record['gitTree'], inventory=inventory, selected=group)
        return write_slice(args.build_bundles/f'slice-{index}.tar.gz', plan, root, sorted(files))

    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=min(args.count, os.cpu_count() or 1)) as pool:
        record['slices'] = list(pool.map(pack, range(args.count)))
    record['packSeconds'] = time.monotonic() - started
    return 0


def run_bundle(args, record):
    root = Path.cwd().resolve()
    with tarfile.open(args.from_bundle, 'r:gz') as tar:
        plan = read_plan(tar)
        if (plan['index'], plan['count']) != (args.shard, args.count):
            raise ValueError(f"bundle is shard {plan['index']} of {plan['count']}, not {args.shard} of {args.count}")
        if Path(plan['workspaceRoot']).resolve() != root:
            raise ValueError(f"bundle built in {plan['workspaceRoot']}, shard runs in {root}")
        head, tree = git_identity(root, required=True)
        if head != plan['gitHead']:
            raise ValueError(f"bundle built at {plan['gitHead']}, shard HEAD is {head}")
        record.update(gitHead=head, gitTree=tree, buildSeconds=0.0)
        target_dir = Path(plan['targetDirectory'])
        members = safe_members(tar, target_dir.relative_to(Path(plan['workspaceRoot'])))
        started = time.monotonic()
        extract_members(tar, members, root)
        record['extractSeconds'] = time.monotonic() - started
    (target_dir/'tmp').mkdir(parents=True, exist_ok=True)
    record['inventory'] = [inventory_view(x) for x in plan['inventory']]
    record['selected'] = [inventory_view(x) for x in plan['selected']]
    with tempfile.TemporaryDirectory(prefix='man518-tripwire-') as tmp:
        tripwire = make_tripwire(tmp)
        runs = [(item, root/item['cwd'], run_env(item, tripwire)) for item in plan['selected']]
        # MAN-498: list before execution, on the host that runs the tests.
        for item, cwd, env in runs:
            list_item(item, cwd, env, expected=item)
        for item, cwd, env in runs:
            record['results'].append(run_item(item, cwd, env))
    return int(any(x['exitCode'] for x in record['results']))


def run_legacy(args, record):
    meta = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version=1']))
    record['gitHead'], record['gitTree'] = git_identity(meta['workspace_root'], required=False)
    items = parse_artifacts(cargo_build(record))
    if not items:
        raise ValueError('empty workspace inventory')
    packages = {p['id']: p for p in meta['packages']}
    sysroot = subprocess.check_output(['rustc', '--print', 'target-libdir'], text=True).strip()
    for item in items:
        package = packages[item['package']]
        env = child_env(package, Path(meta['target_directory']), sysroot)
        cwd = Path(package['manifest_path']).parent
        item['tests'], item['ignored'] = list_item(item, cwd, env)
    groups = assign(items, load_weights(), args.count)
    # All groups are computed on each host; assert complete/disjoint placement before running.
    record['inventory'] = items
    record['selected'] = groups[args.shard]
    for item in ([] if args.inventory_only else record['selected']):
        package = packages[item['package']]
        cwd = Path(package['manifest_path']).parent
        record['results'].append(run_item(item, cwd, child_env(package, Path(meta['target_directory']), sysroot)))
    return int(any(x['exitCode'] for x in record['results']))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--count', type=int, default=8)
    parser.add_argument('--shard', type=int, default=0)
    parser.add_argument('--output', type=Path, required=True)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--audit', type=Path, help='JSON bundle: shards, support, workflowStarted, completed, reference, optional build')
    mode.add_argument('--inventory-only', action='store_true')
    mode.add_argument('--build-bundles', type=Path, metavar='DIR', help='build once and write one slice per shard into DIR')
    mode.add_argument('--from-bundle', type=Path, metavar='FILE', help="run this shard's slice without a compiler")
    args = parser.parse_args()
    if args.audit:
        bundle = json.loads(args.audit.read_text())
        elapsed = audit_evidence(bundle['shards'], bundle['support'], bundle['workflowStarted'], bundle['completed'],
                                 bundle['reference'], bundle.get('build'))
        args.output.write_text(json.dumps(dict(coverage='complete', elapsedSeconds=elapsed)) + '\n')
        return 0
    if args.count < 2 or not 0 <= args.shard < args.count:
        parser.error('require count >= 2 and 0 <= shard < count')
    record = dict(version=1, index=args.shard, count=args.count, started=time.time(), results=[])
    record['runner'] = {key: os.environ.get(key) for key in ('RUNNER_OS', 'RUNNER_ARCH', 'GITHUB_RUN_ID', 'GITHUB_SHA')}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    try:
        if args.build_bundles:
            return build_bundles(args, record)
        if args.from_bundle:
            return run_bundle(args, record)
        return run_legacy(args, record)
    except (ValueError, RuntimeError, subprocess.CalledProcessError, KeyError, OSError, tarfile.TarError) as error:
        record['error'] = str(error)
        print(error, file=sys.stderr)
        return 1
    finally:
        record['completed'] = time.time()
        args.output.write_text(json.dumps(record, indent=2) + '\n')


if __name__ == '__main__':
    sys.exit(main())
