"""Portable native bundle contracts, run on Unix supports and the Windows PR smoke."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import types
import unittest
from unittest import mock

from scripts.tests.test_ci_shards import SCRIPT, ROOT, canonical, fixture_crate, git, s


class WindowsBundleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='man565-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()/'ws'
        self.root.mkdir()
        fixture_crate(self.root)
        # Three integration binaries; aaa and ccc share shard 0 with equal weights.
        (self.root/'tests/integration.rs').rename(self.root/'tests/bbb.rs')
        (self.root/'tests/aaa.rs').write_text('#[test] fn first() {}', encoding='utf-8')
        (self.root/'tests/ccc.rs').write_text('#[test] fn later() { std::fs::write("passed-marker", "ok").unwrap(); }', encoding='utf-8')
        (self.root/'.gitignore').write_text('/target\n/bundles\n*.json\n/passed-marker\n', encoding='utf-8')
        git(self.root, 'init', '-q')
        git(self.root, 'add', '-A')
        git(self.root, 'commit', '-qm', 'fixture')

    def command(self, *args):
        return subprocess.run([sys.executable, SCRIPT, '--count', '2', *args],
                              cwd=self.root, env=dict(os.environ, PYTHONIOENCODING='ascii'),
                              capture_output=True, text=True, encoding='utf-8')

    def build(self):
        proc = self.command('--build-bundles', 'bundles', '--output', 'build.json')
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        record = json.loads((self.root/'build.json').read_text(encoding='utf-8'))
        shutil.rmtree(self.root/'target')
        return record

    def run_slice(self, index, *args):
        out = f'shard-{index}.json'
        proc = self.command('--shard', str(index), '--from-bundle', f'bundles/slice-{index}.tar.gz',
                            '--output', out, *args)
        return proc, json.loads((self.root/out).read_text(encoding='utf-8'))

    def test_bundle_round_trip_and_audit(self):
        build = self.build()
        if sys.platform == 'win32':
            for index in range(2):
                with tarfile.open(self.root/f'bundles/slice-{index}.tar.gz') as tar:
                    plan = s.read_plan(tar)
                    self.assertEqual(plan['tripwireExecutable'], 'target/ci-shard-tripwire.exe')
                    self.assertTrue(tar.getmember(plan['tripwireExecutable']).isfile())
        records = []
        for index in range(2):
            proc, record = self.run_slice(index)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(record['buildSeconds'], 0)
            records.append(record)
        self.assertEqual(sum(len(r['results']) for r in records), 5)
        self.assertTrue(any(x['ignored'] == ['ignored'] for x in build['inventory']))
        self.assertFalse(any(x['name'] == 'excluded' for x in build['inventory']))
        suffix = '.exe' if sys.platform == 'win32' else ''
        self.assertTrue((self.root/f'target/debug/shard-fixture{suffix}').is_file())
        self.assertEqual(s.audit_evidence(records, dict(conclusion='success', doctests='success', started=1, completed=2),
                                         0, 3, build['inventory'], build), 3)
        self.assertEqual(canonical(records[0]['inventory']), canonical(build['inventory']))

    def test_failure_continues_within_slice(self):
        (self.root/'tests/aaa.rs').write_text('#[test] fn first() { panic!("deliberate failure"); }', encoding='utf-8')
        git(self.root, 'commit', '-qam', 'fail first')
        build = self.build()
        keys = next(group for group in build['groups'] if any(k.endswith('::aaa') for k in group))
        self.assertLess(next(i for i, k in enumerate(keys) if k.endswith('::aaa')),
                        next(i for i, k in enumerate(keys) if k.endswith('::ccc')))
        index = build['groups'].index(keys)
        proc, record = self.run_slice(index)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        results = {x['key'].rsplit('::', 1)[1]: x['exitCode'] for x in record['results']}
        self.assertNotEqual(results['aaa'], 0)
        self.assertEqual(results['ccc'], 0)
        self.assertEqual((self.root/'passed-marker').read_text(encoding='utf-8'), 'ok')
        self.assertEqual([x['key'] for x in record['results']], keys)

    def test_utf8_listing_and_repeat(self):
        source = self.root/'tests/ccc.rs'
        source.write_text('''mod 漢字 {
use std::net::TcpListener;
#[test]
fn socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    assert!(listener.local_addr().unwrap().port() > 0);
    println!("漢字");
}
}
''', encoding='utf-8')
        git(self.root, 'commit', '-qam', 'socket base')
        base = git(self.root, 'rev-parse', 'HEAD')
        source.write_text(source.read_text(encoding='utf-8').replace('println!("漢字")', 'println!("漢字 ✓")'), encoding='utf-8')
        git(self.root, 'commit', '-qam', 'unicode edit')
        build = self.build()
        index = next(i for i, group in enumerate(build['groups']) if any(k.endswith('::ccc') for k in group))
        proc, record = self.run_slice(index, '--repeat-base', base)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertTrue(any('漢字::socket' in x['tests'] for x in record['inventory']))
        self.assertEqual([x['test'] for x in record['repeatPlan']['selected']], ['漢字::socket'])
        self.assertEqual([(x['test'], x['passes'], x['ok']) for x in record['repeatResults']], [('漢字::socket', 10, True)])

    def test_native_tripwires(self):
        # Compile the caller before preparing the guard, exactly as the build/shard split does.
        suffix = '.exe' if sys.platform == 'win32' else ''
        source = self.root/'probe.rs'
        source.write_text(r'''
fn main() {
    let mut tools = vec!["cargo".to_string(), "rustc".to_string(), "rustup".to_string()];
    if cfg!(windows) {
        tools.extend(["cargo.exe", "rustc.exe", "rustup.exe"].map(String::from));
    }
    tools.push(std::env::var("CARGO").unwrap());
    for tool in tools {
        let out = std::process::Command::new(&tool).arg("--version").output().unwrap();
        assert_eq!(out.status.code(), Some(97), "{tool}");
        assert!(String::from_utf8(out.stderr).unwrap().contains("MAN-518 tripwire: a shard invoked"));
    }
}
''', encoding='utf-8')
        probe = self.root/f'probe{suffix}'
        subprocess.run(['rustc', str(source), '-o', str(probe)], check=True, capture_output=True)
        helper = self.root/'helper.exe'
        subprocess.run(['rustc', str(ROOT/'scripts/ci-shard-tripwire.rs'), '-o', str(helper)], check=True, capture_output=True)
        guard = self.root/'guard'
        guard.mkdir()
        with mock.patch.object(s.subprocess, 'run', side_effect=AssertionError('compiler in make_tripwire')):
            s.make_tripwire(guard, helper if sys.platform == 'win32' else None)
        env = s.run_env(dict(env={'CARGO_PKG_NAME': 'fixture'}, libraryPath=[]), guard)
        self.assertTrue(env['CARGO'].endswith(f'cargo{suffix}'))
        proc = subprocess.run([str(probe)], cwd=guard, env=env, capture_output=True, text=True, encoding='utf-8')
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_missing_tripwire_fails_closed(self):
        build = self.build()
        path = self.root/'bundles/slice-0.tar.gz'
        with tarfile.open(path) as tar:
            plan = s.read_plan(tar)
            members = s.safe_members(tar, 'target')
            for value in (None, '../helper.exe', '/helper.exe', 'src/helper.exe', 'target/missing.exe'):
                bad = dict(plan)
                if value is None:
                    bad.pop('tripwireExecutable', None)
                else:
                    bad['tripwireExecutable'] = value
                with self.subTest(value=value), self.assertRaises(ValueError):
                    s.validate_tripwire(bad, self.root, self.root/'target', members)
        # Even on Unix, inject Windows validation and prove no executable is listed/run.
        plan.pop('tripwireExecutable', None)
        missing = self.root/'missing.tar.gz'
        s.write_slice(missing, plan, self.root, [])
        args = types.SimpleNamespace(from_bundle=missing, shard=0, count=2, repeat_base=None)
        previous = Path.cwd()
        try:
            os.chdir(self.root)
            with mock.patch.object(s.sys, 'platform', 'win32'), mock.patch.object(s, 'list_item', side_effect=AssertionError('listed before validation')):
                with self.assertRaisesRegex(ValueError, 'missing Windows tripwire'):
                    s.run_bundle(args, dict(results=[]))
        finally:
            os.chdir(previous)
        self.assertTrue(build['inventory'])

    def test_run_bundle_never_calls_a_compiler(self):
        self.build()
        original = s.subprocess.run
        calls = []
        def spy(command, *args, **kwargs):
            calls.append(command)
            self.assertNotIn(Path(command[0]).stem.lower(), s.TRIPWIRE_TOOLS)
            return original(command, *args, **kwargs)
        args = types.SimpleNamespace(from_bundle=self.root/'bundles/slice-0.tar.gz', shard=0, count=2, repeat_base=None)
        before = Path.cwd()
        try:
            os.chdir(self.root)
            with mock.patch.object(s.subprocess, 'run', spy):
                self.assertEqual(s.run_bundle(args, dict(results=[])), 0)
        finally:
            os.chdir(before)
        self.assertTrue(calls)


if __name__ == '__main__':
    unittest.main()
