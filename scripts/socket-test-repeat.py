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
    """Make `base` available for a tree diff; a shallow CI checkout lacks it."""
    if not base or base == ZERO_SHA or not re.fullmatch(r'[0-9a-f]{40}', base):
        raise ValueError(f'repeat base {base!r} is not a commit SHA: socket-test repetition is unverified')
    probe = subprocess.run(['git', 'cat-file', '-e', f'{base}^{{commit}}'], cwd=root,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if probe.returncode:
        fetched = subprocess.run(['git', 'fetch', '--no-tags', '--depth=1', 'origin', base], cwd=root)
        if fetched.returncode:
            raise ValueError(f'repeat base {base} is unavailable: socket-test repetition is unverified')


def changed_lines(root, base, head='HEAD'):
    """{new path: set of changed new-side lines (or {0} for a pure deletion point)} plus removed paths."""
    raw = git(root, 'diff', '--no-color', '--find-renames', '--unified=0', base, head, '--', '*.rs')
    changed, removed, path = {}, [], None
    old = None
    for line in raw.splitlines():
        if line.startswith('--- '):
            old = None if line == '--- /dev/null' else line[6:]
        elif line.startswith('+++ '):
            path = None if line == '+++ /dev/null' else line[6:]
            if path is None and old is not None:
                removed.append(old)
            elif path is not None:
                changed.setdefault(path, set())
        elif path is not None:
            hunk = _HUNK.match(line)
            if hunk:
                start, count = int(hunk.group(1)), int(hunk.group(2) or 1)
                # A pure deletion (count 0) changes the line it follows.
                changed[path].update(range(start, start + count) if count else {max(start, 1)})
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
    """Changed lines other than blank and `//` comment lines, which change no test."""
    source = text.splitlines()
    return {n for n in lines if n > len(source) or not (source[n - 1].strip() == '' or source[n - 1].lstrip().startswith('//'))}


def _binds(paths):
    return any(any(b in p.read_text(errors='replace') for b in SOCKET_BINDS) for p in paths if p.exists())


def select(root, item, changed):
    """[(test name, reason)] for one inventory item; `changed` is {repo-relative path: lines}."""
    root = Path(root).resolve()
    package_dir = (root / item['cwd']).resolve()
    tests = item.get('tests', [])
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
            if path != own:
                return [(t, f'helper {rel} changed') for t in tests]
            text = path.read_text(errors='replace')
            lines = _meaningful(text, lines)
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
            test_mod = _cfg_test_lines(text)
            if (lines - covered) & test_mod:
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


def plan(root, items, base, head='HEAD'):
    """{'base', 'removed', 'selected': [{key, test, reason}], 'noop': reason or None}."""
    ensure_base(root, base)
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
