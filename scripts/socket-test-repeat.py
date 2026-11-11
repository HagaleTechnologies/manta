#!/usr/bin/env python3
"""MAN-523: run the socket tests a change touches ten more times; every run must pass.

`scripts/ci-shards.py --repeat-base SHA` imports this module. After a shard's ordinary
whole-executable run, it selects the tests the diff from SHA to HEAD adds or edits in
targets that bind sockets, and runs each one REPEAT_COUNT times as a fresh
`<executable> --exact <name>` invocation, with the shard's working directory and
environment. A failure stays a failure: nothing is retried.

Selection, per test executable in the shard's inventory:
- a candidate is a target whose sources contain `TcpListener::bind` or `UdpSocket::bind`
  (an integration target with the helpers it includes; a lib or bin with its whole `src/`),
  plus the named seeds below;
- a changed line inside a test function selects that test;
- any other changed line in a candidate's sources (a helper, an import, a `mod`) selects
  every test of that target, or for `src/`, every test of the changed file's module;
- production code outside `#[cfg(test)]` code selects nothing.
"""
import functools
import json
import re
import subprocess
import sys
import time
from pathlib import Path

REPEAT_COUNT = 10
SOCKET_BINDS = ('TcpListener::bind', 'UdpSocket::bind')
# Not a socket test, but MAN-523 names its report as unexplained: repeat it when it changes.
SEEDS = {('manta-input', 'recording')}
ZERO_SHA = '0' * 40

_TEST_ATTR = re.compile(r'#\[\s*(?:[\w:]+::)?test\b')
_FN = re.compile(r'\bfn\s+([A-Za-z_][A-Za-z0-9_]*)')
_HUNK = re.compile(r'^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@')
_PATH_ATTR = re.compile(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;')
_MOD = re.compile(r'^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+(\w+)\s*;', re.M)
_PASSED = re.compile(r'^test result: ok\. (\d+) passed;', re.M)


def git(cwd, *args):
    return subprocess.run(['git', *args], cwd=cwd, check=True, text=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout


def ensure_base(root, base):
    """Make `base` available for a tree diff; a shallow CI checkout lacks it. `HEAD^1` names the
    tested merge commit's first parent (a pull_request run's refs/pull/N/merge), which the
    commit object records even when the checkout is shallow. Returns the SHA."""
    if base == 'HEAD^1':
        parents = re.findall(r'^parent ([0-9a-f]{40})$', git(root, 'cat-file', '-p', 'HEAD'), re.M)
        if not parents:
            raise ValueError('HEAD has no parent: socket-test repetition is unverified')
        base = parents[0]
    if not base or base == ZERO_SHA or not re.fullmatch(r'[0-9a-f]{40}', base):
        raise ValueError(f'repeat base {base!r} is not a commit SHA: socket-test repetition is unverified')
    probe = subprocess.run(['git', 'cat-file', '-e', f'{base}^{{commit}}'], cwd=root,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if probe.returncode:
        fetched = subprocess.run(['git', 'fetch', '--no-tags', '--depth=1', 'origin', base], cwd=root)
        if fetched.returncode:
            raise ValueError(f'repeat base {base} is unavailable: socket-test repetition is unverified')
    return base


def changed_lines(root, base, head='HEAD'):
    """({new path: changed new-side lines}, removed paths). A pure deletion is recorded as the
    negated number of the line it follows, so a blank line above it cannot hide it."""
    raw = git(root, 'diff', '--no-color', '--find-renames', '--unified=0', base, head, '--', '*.rs')
    changed, removed, path, old, header = {}, [], None, None, False
    for line in raw.splitlines():
        if line.startswith('diff --git '):
            header, path, old = True, None, None
        elif header and line.startswith('--- '):
            old = None if line == '--- /dev/null' else line[6:]
        elif header and line.startswith('+++ '):
            path = None if line == '+++ /dev/null' else line[6:]
            if path is None and old is not None:
                removed.append(old)
            elif path is not None:
                changed.setdefault(path, set())
        elif line.startswith('@@'):
            # Hunk bodies follow; a body line may itself start with `--- ` or `+++ `.
            header = False
            hunk = _HUNK.match(line)
            if hunk and path is not None:
                start, count = int(hunk.group(1)), int(hunk.group(2) or 1)
                changed[path].update(range(start, start + count) if count else {-max(start, 1)})
    return changed, removed


def _blank_strings_and_comments(text):
    """Text with comment bodies and string contents replaced by spaces, newlines kept."""
    out, i, n = [], 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith('//', i):
            j = text.find('\n', i)
            j = n if j < 0 else j
            out.append(' ' * (j - i))
            i = j
        elif text.startswith('/*', i):
            j = text.find('*/', i + 2)
            j = n if j < 0 else j + 2
            out.append(re.sub(r'[^\n]', ' ', text[i:j]))
            i = j
        elif c == 'r' and re.match(r'r#*"', text[i:i + 8]) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == '_')):
            hashes = len(re.match(r'r(#*)"', text[i:]).group(1))
            end = '"' + '#' * hashes
            j = text.find(end, i + 2 + hashes)
            j = n if j < 0 else j + len(end)
            out.append(re.sub(r'[^\n]', ' ', text[i:j]))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == '\\' else 1
            j = min(j + 1, n)
            out.append(re.sub(r'[^\n]', ' ', text[i:j]))
            i = j
        elif c == "'" and re.match(r"'(?:\\.|[^\\'])'", text[i:i + 4]):
            m = re.match(r"'(?:\\.|[^\\'])'", text[i:])
            out.append(' ' * len(m.group(0)))
            i += len(m.group(0))
        else:
            out.append(c)
            i += 1
    return ''.join(out)


def test_functions(text):
    """[(name, first line, last line)] for every `#[test]`-style function, 1-based, attributes included."""
    code = _blank_strings_and_comments(text)
    starts = [0]
    for i, c in enumerate(code):
        if c == '\n':
            starts.append(i + 1)

    def line_of(offset):
        lo, hi = 0, len(starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if starts[mid] <= offset:
                lo = mid
            else:
                hi = mid - 1
        return lo + 1

    found = []
    for attr in _TEST_ATTR.finditer(code):
        fn = _FN.search(code, attr.end())
        if not fn:
            continue
        brace = code.find('{', fn.end())
        if brace < 0:
            continue
        depth, j = 0, brace
        while j < len(code):
            if code[j] == '{':
                depth += 1
            elif code[j] == '}':
                depth -= 1
                if depth == 0:
                    break
            j += 1
        found.append((fn.group(1), line_of(attr.start()), line_of(j)))
    return found


def target_root(tests_dir, target):
    root = tests_dir / f'{target}.rs'
    return (root if root.exists() else tests_dir / target / 'main.rs').resolve()


def integration_closure(tests_dir, target):
    """The target's root file plus every file it includes by `mod` or `#[path]`, conservatively."""
    root = target_root(tests_dir, target)
    seen, todo = set(), [root]
    while todo:
        path = todo.pop()
        if path in seen or not path.exists():
            continue
        seen.add(path)
        text = path.read_text(errors='replace')
        for rel, _ in _PATH_ATTR.findall(text):
            todo.append((path.parent / rel).resolve())
        base = path.parent if path.name in ('main.rs', 'mod.rs') or path == root else path.parent / path.stem
        for name in _MOD.findall(text):
            for candidate in (base / f'{name}.rs', base / name / 'mod.rs'):
                if candidate.exists():
                    todo.append(candidate.resolve())
                    # A directory module's siblings may be included by its `mod.rs`.
                    if candidate.name == 'mod.rs':
                        todo.extend(p.resolve() for p in candidate.parent.rglob('*.rs'))
    return {p.resolve() for p in seen}


def module_prefix(src_dir, path):
    """`list_http::` for src/list_http.rs, `a::b::` for src/a/b.rs, '' for main.rs/lib.rs."""
    parts = list(path.relative_to(src_dir).with_suffix('').parts)
    if parts and parts[-1] in ('mod', 'main', 'lib'):
        parts.pop()
    return ''.join(f'{p}::' for p in parts)


def _meaningful(text, lines):
    """Changed lines other than added blank and `//` comment lines, which change no test.
    Deletion points (negated) always count."""
    source = text.splitlines()
    out = set()
    for n in lines:
        if n < 0:
            out.add(-n)
        elif n > len(source) or not (source[n - 1].strip() == '' or source[n - 1].lstrip().startswith('//')):
            out.add(n)
    return out


def _file_binds(path):
    """Whether the file's code, not its strings or comments, binds a socket."""
    return any(b in _code(path) for b in SOCKET_BINDS)


def _binds(paths):
    return any(_file_binds(p) for p in paths if p.exists())


def _test_only_file(src_dir, path):
    """Whether a `src/` file is a whole module declared `#[cfg(test)] mod <stem>;` (test_ports.rs)."""
    stem = path.parent.name if path.name == 'mod.rs' else path.stem
    pattern = re.compile(r'#\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*(?:#\[[^\]]*\]\s*)*(?:pub(?:\([^)]*\))?\s+)?mod\s+'
                         + re.escape(stem) + r'\s*;')
    return any(pattern.search(_code(p)) for p in src_dir.rglob('*.rs') if p != path)


@functools.lru_cache(maxsize=None)
def _code(path):
    return _blank_strings_and_comments(path.read_text(errors='replace'))


def select(root, item, changed):
    """[(test name, reason)] for one inventory item; `changed` is {repo-relative path: lines}.
    Ignored tests are never selected: `--exact` without `--include-ignored` would run none."""
    root = Path(root).resolve()
    package_dir = (root / item['cwd']).resolve()
    ignored = set(item.get('ignored', []))
    tests = [t for t in item.get('tests', []) if t not in ignored]
    package_name = Path(item['cwd']).name
    picked = {}
    if item['kind'] == 'test':
        tests_dir = package_dir / 'tests'
        closure = integration_closure(tests_dir, item['name'])
        if not ((package_name, item['name']) in SEEDS or _binds(closure)):
            return []
        own = target_root(tests_dir, item['name'])
        for rel, lines in changed.items():
            path = (root / rel).resolve()
            if path not in closure:
                continue
            text = path.read_text(errors='replace')
            lines = _meaningful(text, lines)
            if not lines:
                continue
            if path != own:
                return [(t, f'helper {rel} changed') for t in tests]
            spans = test_functions(text)
            covered = set()
            for name, first, last in spans:
                hit = lines & set(range(first, last + 1))
                covered |= hit
                if hit:
                    for t in tests:
                        if t == name or t.endswith(f'::{name}'):
                            picked[t] = f'{rel}: {name} changed'
            if lines - covered:
                return [(t, f'{rel}: code outside a test function changed') for t in tests]
    else:
        src_dir = package_dir / 'src'
        if not src_dir.is_dir() or not _binds(src_dir.rglob('*.rs')):
            return []
        for rel, lines in changed.items():
            path = (root / rel).resolve()
            if src_dir not in path.parents:
                continue
            text = path.read_text(errors='replace')
            lines = _meaningful(text, lines)
            prefix = module_prefix(src_dir, path)
            spans = test_functions(text)
            covered = set()
            for name, first, last in spans:
                hit = lines & set(range(first, last + 1))
                covered |= hit
                if hit:
                    for t in tests:
                        if t.startswith(prefix) and (t == name or t.endswith(f'::{name}')):
                            picked[t] = f'{rel}: {name} changed'
            rest = lines - covered
            if not rest:
                continue
            if _test_only_file(src_dir, path):
                # A shared test helper module: its own tests and those of every module naming it.
                stem = path.parent.name if path.name == 'mod.rs' else path.stem
                prefixes = {prefix} | {module_prefix(src_dir, p) for p in src_dir.rglob('*.rs')
                                       if p != path and f'{stem}::' in _code(p)}
                for t in tests:
                    if any(t.startswith(x) for x in prefixes):
                        picked.setdefault(t, f'{rel}: test helper module changed')
            elif rest & _cfg_test_lines(text):
                for t in tests:
                    if t.startswith(prefix):
                        picked.setdefault(t, f'{rel}: test-only code outside a test function changed')
    return sorted(picked.items())


def _cfg_test_lines(text):
    """Lines inside `#[cfg(test)]` items (1-based)."""
    code = _blank_strings_and_comments(text)
    lines = set()
    for attr in re.finditer(r'#\[\s*cfg\s*\(\s*test\s*\)\s*\]', code):
        first = code.count('\n', 0, attr.start()) + 1
        brace, semi = code.find('{', attr.end()), code.find(';', attr.end())
        if brace < 0 or (0 <= semi < brace):
            lines.add(first)
            continue
        depth, j = 0, brace
        while j < len(code):
            if code[j] == '{':
                depth += 1
            elif code[j] == '}':
                depth -= 1
                if depth == 0:
                    break
            j += 1
        lines.update(range(first, code.count('\n', 0, j) + 2))
    return lines


def rust_changed(root, base, head='HEAD'):
    """Whether the diff touches any Rust file: the cheap check before building an inventory."""
    changed, removed = changed_lines(root, ensure_base(root, base), head)
    return bool(changed or removed)


def plan(root, items, base, head='HEAD'):
    """{'base', 'removed', 'selected': [{key, test, reason}], 'noop': reason or None}."""
    base = ensure_base(root, base)
    changed, removed = changed_lines(root, base, head)
    selected = []
    for item in items:
        for test, reason in select(root, item, changed):
            selected.append(dict(key=item['key'], test=test, reason=reason))
    noop = None if selected else ('no relevant change' if changed or removed else 'no Rust change')
    return dict(base=base, removed=removed, selected=selected, noop=noop)


def repeat(item, test, cwd, env, count=REPEAT_COUNT, runner=subprocess.run):
    """Run one test `count` times; return its evidence. Never retries."""
    if count < REPEAT_COUNT:
        raise ValueError(f'repeat count {count} is below {REPEAT_COUNT}')
    command = [item['executable'], '--exact', test]
    runs = []
    for iteration in range(1, count + 1):
        started = time.monotonic()
        proc = runner(command, cwd=cwd, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        output = proc.stdout or ''
        passed = _PASSED.search(output)
        ok = proc.returncode == 0 and passed is not None and int(passed.group(1)) == 1
        runs.append(dict(iteration=iteration, exitCode=proc.returncode, seconds=time.monotonic() - started, ok=ok))
        if not ok:
            sys.stdout.write(output)
            print(f'::error title=Socket test repeat failed::{test} in {item["key"]} failed on iteration '
                  f'{iteration} of {count}', flush=True)
            break
    passes = sum(r['ok'] for r in runs)
    print(f"Repeated {test} in {item['key']}: {passes}/{count} passed", flush=True)
    return dict(key=item['key'], test=test, command=command, cwd=str(cwd), count=count, passes=passes,
                runs=runs, ok=passes == count)


def audit_repeats(repeat_plan, results, count=REPEAT_COUNT):
    """Every planned (key, test) ran `count` times and passed each time."""
    planned = {(x['key'], x['test']) for x in repeat_plan['selected']}
    ran = [(x['key'], x['test']) for x in results]
    if len(ran) != len(set(ran)):
        raise ValueError(f'a socket test was repeated twice: {ran}')
    if set(ran) != planned:
        raise ValueError(f'repeat results {sorted(set(ran))} differ from the plan {sorted(planned)}')
    short = [x for x in results if x['count'] < count or x['passes'] != x['count'] or not x['ok']]
    if short:
        raise ValueError(f'socket tests did not pass {count} times: {[(x["test"], x["passes"]) for x in short]}')


if __name__ == '__main__':
    # Dry run: print what a diff would select for an inventory JSON (ci-shards.py evidence).
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base', required=True)
    parser.add_argument('--inventory', type=Path, required=True)
    args = parser.parse_args()
    evidence = json.loads(args.inventory.read_text())
    print(json.dumps(plan(Path.cwd(), evidence['inventory'], args.base), indent=2))
