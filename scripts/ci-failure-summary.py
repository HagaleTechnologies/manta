#!/usr/bin/env python3
"""Summarize Windows CI evidence without changing the job verdict."""
import argparse
import json
import os
from pathlib import Path
import re
import sys

REGISTRY = json.loads(Path(__file__).with_name('ci-flake-classes.json').read_text(encoding='utf-8'))


def classify(target, test, content):
    for entry in REGISTRY['tests']:
        if entry['target'] == target and entry['test'] == test:
            return entry['class'], f"registry: {entry['ticket']} ({entry['fix']})"
    for entry in REGISTRY['signatures']:
        if re.search(entry['regex'], content):
            return entry['class'], f"signature: {entry['regex']}"
    return 'unknown', 'no known rule'


def records(paths):
    for path in paths:
        path = Path(path)
        for file in sorted(path.rglob('*.json') if path.is_dir() else [path]):
            try:
                yield file, json.loads(file.read_text(encoding='utf-8'))
            except (OSError, ValueError) as exc:
                yield file, {'readError': str(exc)}


def render(os_name, evidence, shard=None, expect_shards=None, needs=None):
    rows, other, details = [], [], []
    seen, binaries = set(), 0
    for path, record in evidence:
        if not isinstance(record, dict):
            other.append(f'{path}: unrecognized evidence (not a JSON object)')
            continue
        if 'readError' in record:
            other.append(f'{path}: evidence unreadable: {record["readError"]}')
            continue
        if record.get('role') == 'build':
            if record.get('error') or record.get('exitCode', 0):
                other.append(f'build: {record.get("error", "failed")}')
        elif 'doctests' in record:
            for field in ('conclusion', 'doctests'):
                if record.get(field) != 'success':
                    other.append(f'support {field}: {record.get(field, "missing")}')
        elif 'index' in record and 'results' in record:
            index = record['index']
            seen.add(index)
            if record.get('error'):
                other.append(f'shard {index} runner error: {record["error"]}')
            for result in record['results']:
                binaries += 1
                key = result.get('key', 'unknown::unknown')
                target = key.rsplit('::', 1)[-1]
                exit_code = result.get('exitCode', 0)
                if result.get('parseError'):
                    other.append(f'shard {index} {target}: failure parser error: {result["parseError"]}')
                if exit_code == 97:
                    other.append(f'shard {index} {target}: tripwire exit 97')
                if not exit_code:
                    continue
                names = result.get('failedTests', [])
                if result.get('omittedFailedTests'):
                    other.append(f'shard {index} {target}: {result["omittedFailedTests"]} additional failed tests omitted from evidence')
                if not names:
                    other.append(f'shard {index} {target}: exit {exit_code} (0x{exit_code & 0xffffffff:08X}); {result.get("outputTail", "no failure list")[:500]}')
                for name in names:
                    excerpt = result.get('failureExcerpts', {}).get(name, '')
                    label, rule = classify(target, name, excerpt)
                    rows.append((index, target, name, label, rule))
                    details.append((name, excerpt))
            for repeat in record.get('repeatResults', []):
                if not repeat.get('ok'):
                    # A failed repeat is a failing test: name its binary,
                    # classify it from the failed iteration's excerpt
                    # (socket-test-repeat.py writes it), and show the excerpt.
                    key = repeat.get('key', 'unknown::unknown')
                    target = key.rsplit('::', 1)[-1]
                    name = repeat.get('test', 'unknown')
                    excerpt = next((run.get('excerpt', '') for run in repeat.get('runs', [])
                                    if isinstance(run, dict) and not run.get('ok')), '')
                    label, rule = classify(target, name, excerpt)
                    other.append(f'shard {index} socket-test repeat failed: {name} in {target}; class: {label} ({rule})')
                    if excerpt:
                        details.append((f'{name} (repeat)', excerpt))
        else:
            other.append(f'{path}: unrecognized evidence')
    if shard is not None and shard not in seen:
        other.append(f'shard {shard}: missing evidence')
    if expect_shards is not None:
        for index in range(expect_shards):
            if index not in seen:
                other.append(f'shard {index}: missing evidence')
    if needs:
        for name, item in needs.items():
            if item.get('result') != 'success':
                other.append(f'{name}: {item.get("result", "missing")}')
    title = f'### Windows ({os_name}): '
    if not rows and not other:
        return title + f'no failures ({len(seen)} shards, {binaries} binaries)\n'
    lines = [title + f'{len(rows)} failing tests, {len(other)} other failures', '', '| Shard | Binary | Test | Class | Matched by |', '|---|---|---|---|---|']
    for index, target, name, label, rule in rows[:50]:
        safe = lambda value: str(value).replace('|', '\\|').replace('\n', ' ')
        lines.append(f'| {index} | {safe(target)} | {safe(name)} | {label} | {safe(rule)} |')
    if len(rows) > 50:
        lines.append(f'{len(rows)-50} more failing tests omitted from table')
    for failure in other:
        lines.append(f'- {failure}')
    for name, excerpt in details[:50]:
        lines.extend([f'<details><summary>{name}</summary>', '', '```text', excerpt[:4000].replace('```', "'''"), '```', '</details>'])
    return '\n'.join(lines) + '\n'


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument('--os', required=True)
    parser.add_argument('--shard', type=int)
    parser.add_argument('--expect-shards', type=int)
    parser.add_argument('--needs-env')
    parser.add_argument('--append', type=Path)
    parser.add_argument('evidence', nargs='*')
    args = parser.parse_args(argv)
    needs = json.loads(os.environ[args.needs_env]) if args.needs_env else None
    output = render(args.os, list(records(args.evidence)), args.shard, args.expect_shards, needs)
    if args.append:
        with args.append.open('a', encoding='utf-8') as file:
            file.write(output)
    else:
        sys.stdout.write(output)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
