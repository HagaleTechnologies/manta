#!/usr/bin/env python3
"""Run complete Cargo test executables, preserving libtest's process-local locks.

Doctests are deliberately a separate required `cargo test --workspace --doc --locked`.
Timing weights affect placement only. Every discovered executable is always assigned.
Runs on Linux, macOS and Windows (MAN-517), and sets the dynamic-library search path the
way `cargo test` does on each.
"""
import argparse
import collections
import json
import os
from pathlib import Path
import subprocess
import shutil
import sys
import time


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


def assign(items, weights, count):
    if count < 2:
        raise ValueError('at least two shards are required')
    shards, loads = [[] for _ in range(count)], [0] * count
    for item in sorted(items, key=lambda x: (-weights.get(x['name'], 30), x['key'])):
        weight = weights.get(item['name'], 30)
        index = 0 if item['name'] == 'golden_v8_v8w' else min(range(1, count), key=lambda i: (loads[i], i))
        shards[index].append(item)
        loads[index] += weight
    assert collections.Counter(x['key'] for group in shards for x in group) == collections.Counter(x['key'] for x in items)
    return shards


def dylib_path_variable(platform=sys.platform):
    """Where Cargo puts the dynamic-library search path for `cargo test` (cargo-util's
    dylib_path_envvar): PATH on Windows, DYLD_FALLBACK_LIBRARY_PATH on macOS, else LD_LIBRARY_PATH."""
    if platform == 'win32':
        return 'PATH'
    return 'DYLD_FALLBACK_LIBRARY_PATH' if platform == 'darwin' else 'LD_LIBRARY_PATH'


def child_env(package, target_dir, rustlib, platform=sys.platform):
    env = os.environ.copy()
    env.update(CARGO_MANIFEST_DIR=str(Path(package['manifest_path']).parent),
               CARGO_MANIFEST_PATH=package['manifest_path'], CARGO_PKG_NAME=package['name'],
               CARGO_PKG_VERSION=package['version'], CARGO=shutil.which('cargo') or 'cargo')
    for variable, field in {
        'AUTHORS': 'authors', 'DESCRIPTION': 'description', 'HOMEPAGE': 'homepage',
        'LICENSE': 'license', 'LICENSE_FILE': 'license_file', 'README': 'readme',
        'REPOSITORY': 'repository', 'RUST_VERSION': 'rust_version',
    }.items():
        value = package.get(field)
        env[f'CARGO_PKG_{variable}'] = ':'.join(value) if isinstance(value, list) else str(value or '')
    version = package['version'].split('+', 1)[0]
    release, _, pre = version.partition('-')
    major, minor, patch = release.split('.')
    env.update(CARGO_PKG_VERSION_MAJOR=major, CARGO_PKG_VERSION_MINOR=minor,
               CARGO_PKG_VERSION_PATCH=patch, CARGO_PKG_VERSION_PRE=pre)
    # Cargo includes build-script OUT_DIR entries on the dynamic-library path. Include all
    # native outputs from this build, as well as deps and the toolchain's Rust libraries.
    paths = [str(p) for p in (target_dir/'debug/build').glob('*/out')]
    paths += [str(target_dir/'debug/deps'), str(target_dir/'debug'), rustlib, str(Path(rustlib).parents[2])]
    variable = dylib_path_variable(platform)
    env[variable] = os.pathsep.join(paths + ([env[variable]] if env.get(variable) else []))
    return env


def listing(item, cwd, env, ignored=False):
    command = [item['executable'], '--list', '--format', 'terse'] + (['--ignored'] if ignored else [])
    proc = subprocess.run(command, cwd=cwd, env=env, encoding='utf-8', capture_output=True, check=True)
    names = []
    for line in proc.stdout.splitlines():
        if not line:
            continue
        if not line.endswith((': test', ': benchmark')):
            raise ValueError(f"unsupported listing for {item['key']}: {line}")
        names.append(line.rsplit(': ', 1)[0])
    return sorted(names)


def audit_evidence(records, support, workflow_started, completed, reference=None):
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--count', type=int, default=8)
    parser.add_argument('--shard', type=int, default=0)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--audit', type=Path, help='JSON bundle: shards, support, workflowStarted, completed')
    parser.add_argument('--inventory-only', action='store_true')
    args = parser.parse_args()
    if args.audit:
        bundle = json.loads(args.audit.read_text(encoding='utf-8'))
        elapsed = audit_evidence(bundle['shards'], bundle['support'], bundle['workflowStarted'], bundle['completed'], bundle['reference'])
        args.output.write_text(json.dumps(dict(coverage='complete', elapsedSeconds=elapsed)) + '\n')
        return 0
    if args.count < 2 or not 0 <= args.shard < args.count:
        parser.error('require count >= 2 and 0 <= shard < count')
    record = dict(version=1, index=args.shard, count=args.count, started=time.time(), results=[])
    record['runner'] = {key: os.environ.get(key) for key in ('RUNNER_OS', 'RUNNER_ARCH', 'GITHUB_RUN_ID', 'GITHUB_SHA')}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    try:
        meta = json.loads(subprocess.check_output(['cargo', 'metadata', '--no-deps', '--locked', '--format-version=1']))
        started = time.monotonic()
        proc = subprocess.run(['cargo', 'test', '--workspace', '--locked', '--no-run', '--message-format=json'], encoding='utf-8', stdout=subprocess.PIPE)
        record['buildSeconds'] = time.monotonic() - started
        if proc.returncode:
            raise RuntimeError(f'Cargo build exited {proc.returncode}')
        items = parse_artifacts(proc.stdout)
        if not items:
            raise ValueError('empty workspace inventory')
        packages = {p['id']: p for p in meta['packages']}
        sysroot = subprocess.check_output(['rustc', '--print', 'target-libdir'], encoding='utf-8').strip()
        for item in items:
            package = packages[item['package']]
            env = child_env(package, Path(meta['target_directory']), sysroot)
            cwd = Path(package['manifest_path']).parent
            item['tests'] = listing(item, cwd, env)
            item['ignored'] = listing(item, cwd, env, True)
        weights = json.loads(Path(__file__).with_name('ci-test-weights.json').read_text(encoding='utf-8'))['weights']
        groups = assign(items, weights, args.count)
        # All groups are computed on each host; assert complete/disjoint placement before running.
        record['inventory'] = items
        record['selected'] = groups[args.shard]
        for item in ([] if args.inventory_only else record['selected']):
            package = packages[item['package']]
            cwd = Path(package['manifest_path']).parent
            command = [item['executable']]
            print(f"Running {item['key']} from {cwd}", flush=True)
            started = time.monotonic()
            result = subprocess.run(command, cwd=cwd, env=child_env(package, Path(meta['target_directory']), sysroot))
            record['results'].append(dict(key=item['key'], command=command, cwd=str(cwd), seconds=time.monotonic()-started, exitCode=result.returncode))
        return int(any(x['exitCode'] for x in record['results']))
    except (ValueError, RuntimeError, subprocess.CalledProcessError, KeyError) as error:
        record['error'] = str(error)
        print(error, file=sys.stderr)
        return 1
    finally:
        record['completed'] = time.time()
        args.output.write_text(json.dumps(record, indent=2) + '\n')


if __name__ == '__main__':
    sys.exit(main())
