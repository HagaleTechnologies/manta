"""Whole-binary CI scheduling contracts (stdlib, including a real Cargo fixture)."""
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('ci_shards', ROOT / 'scripts/ci-shards.py')
s = importlib.util.module_from_spec(spec)
spec.loader.exec_module(s)


def artifact(name='a', kind='test', package='p', executable='/tmp/a', test=True):
    return dict(reason='compiler-artifact', package_id=package,
                target=dict(name=name, kind=[kind]), profile=dict(test=test), executable=executable)


SCRIPT = str(ROOT/'scripts/ci-shards.py')
FIXTURE_TOML = '[package]\nname="shard-fixture"\nversion="0.1.0"\nedition="2021"\n[features]\nextra=[]\n[[test]]\nname="excluded"\nrequired-features=["extra"]\n'
FIXTURE_LIB = '/// ```\n/// assert_eq!(2+2,4);\n/// ```\npub fn f() {}\n#[test] fn unit() {}\n#[test] #[ignore] fn ignored() { panic!("ignored"); }\n'
FIXTURE_INTEGRATION = '#[test] fn integration() { assert_eq!(std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(), "0"); assert_eq!(std::env::var("CARGO_PKG_NAME").unwrap(), "shard-fixture"); assert_eq!(std::fs::read_to_string("data").unwrap(), "fixture"); assert!(std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("data").exists()); assert!(std::process::Command::new(env!("CARGO_BIN_EXE_shard-fixture")).status().unwrap().success()); }'


def git_env(home):
    """No ambient git configuration (as test_ci_trust_boundary.scrubbed_git_env)."""
    env = dict(os.environ, GIT_CONFIG_COUNT='0', GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull, HOME=home)
    for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE'):
        env.pop(key, None)
    return env


def git(cwd, *args):
    return subprocess.run(['git', '-c', 'user.name=MAN-518', '-c', 'user.email=man518@example.invalid',
                           '-c', 'commit.gpgsign=false', *args], cwd=cwd, env=git_env(str(cwd)),
                          check=True, capture_output=True, text=True).stdout.strip()


def fixture_crate(p):
    (p/'src').mkdir(); (p/'tests').mkdir(); (p/'data').write_text('fixture')
    (p/'Cargo.toml').write_text(FIXTURE_TOML)
    (p/'src/lib.rs').write_text(FIXTURE_LIB)
    (p/'src/main.rs').write_text('fn main() { println!("fixture"); }')
    (p/'tests/excluded.rs').write_text('#[test] fn excluded() { panic!(); }')
    (p/'tests/integration.rs').write_text(FIXTURE_INTEGRATION)
    subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=p, check=True, capture_output=True)


def canonical(items):
    return sorted((x['key'], x['tests'], x['ignored']) for x in items)


def no_compiler_path(directory):
    """A PATH holding only python3 and git, so neither cargo, rustc nor rustup resolves."""
    directory.mkdir()
    (directory/'python3').symlink_to(sys.executable)
    (directory/'git').symlink_to(shutil.which('git'))
    return str(directory)


class ShardTests(unittest.TestCase):
    def test_discovery_deduplicates_and_rejects_ambiguity(self):
        a = artifact()
        self.assertEqual(len(s.parse_artifacts('\n'.join(map(json.dumps, [a, a])))), 1)
        for msg in [artifact(kind='custom-build'), artifact(executable='/tmp/b')]:
            with self.assertRaises(ValueError):
                s.parse_artifacts('\n'.join(map(json.dumps, [a, msg])))
        with self.assertRaises(ValueError):
            s.parse_artifacts('{bad')
        self.assertEqual(s.parse_artifacts(json.dumps(artifact(test=False))), [])
        self.assertEqual(s.parse_artifacts(json.dumps(artifact(kind='example', test=False))), [])
        self.assertEqual(s.parse_artifacts(json.dumps(artifact(kind='example')))[0]['kind'], 'example')
        self.assertEqual(s.parse_artifacts(json.dumps(artifact(executable=None))), [])

    def test_complete_disjoint_stable_and_no_reserved_shard(self):
        names = ['golden_v8', 'golden_v8w', 'golden_vc3', 'new', 'a', 'b', 'c', 'd']
        items = s.parse_artifacts('\n'.join(json.dumps(artifact(n)) for n in names))
        shards = s.assign(items, {}, 8)
        self.assertEqual(shards, s.assign(list(reversed(items)), {}, 8))
        # MAN-521: no shard is kept for one binary; equal weights fill every shard once.
        self.assertEqual([len(group) for group in shards], [1] * 8)
        self.assertEqual(sorted(x['key'] for group in shards for x in group), sorted(x['key'] for x in items))
        for count in [0, 1]:
            with self.assertRaises(ValueError):
                s.assign(items, {}, count)
        collision = s.parse_artifacts('\n'.join(json.dumps(artifact(package=p)) for p in ['p', 'q']))
        self.assertEqual(len(collision), 2)

    def test_weights_file_puts_each_golden_scene_on_its_own_shard(self):
        weights = json.loads((ROOT/'scripts/ci-test-weights.json').read_text())['weights']
        self.assertNotIn('golden_v8_v8w', weights)
        names = list(weights) + [f'small{i}' for i in range(20)]
        items = s.parse_artifacts('\n'.join(json.dumps(artifact(n)) for n in names))
        shard_of = {x['name']: i for i, group in enumerate(s.assign(items, weights, 8)) for x in group}
        golden = [shard_of[n] for n in ('golden_v8', 'golden_v8w', 'golden_vc3')]
        self.assertEqual(len(set(golden)), 3, shard_of)

    def test_weight_keys_name_workspace_targets(self):
        weights = json.loads((ROOT/'scripts/ci-test-weights.json').read_text())['weights']
        meta = json.loads(subprocess.run(['cargo', 'metadata', '--no-deps', '--format-version', '1', '--offline'],
                                         cwd=ROOT, check=True, capture_output=True, text=True).stdout)
        targets = {t['name'] for p in meta['packages'] for t in p['targets'] if set(t['kind']) & {'lib', 'test'}}
        self.assertEqual(sorted(set(weights) - targets), [])

    def test_evidence_fails_closed(self):
        inv = [dict(key='a', tests=['ok'], ignored=['ignored'])]
        records = [dict(index=i, count=2, inventory=inv, selected=inv if i == 0 else [],
                        results=[dict(key='a', exitCode=0, seconds=1)] if i == 0 else []) for i in range(2)]
        support = dict(conclusion='success', doctests='success', started=10, completed=50)
        self.assertEqual(s.audit_evidence(records, support, 5, 60), 55)
        for changed in [records[:1], [records[0], records[0]],
                        [records[0], dict(records[1], selected=inv)],
                        [dict(records[0], results=[dict(key='a', exitCode=1)]), records[1]],
                        [records[0], dict(records[1], inventory=[dict(inv[0], ignored=[])])]]:
            with self.assertRaises(ValueError):
                s.audit_evidence(changed, support, 5, 60)
        with self.assertRaises(ValueError):
            s.audit_evidence(records, {}, 5, 60)
        with self.assertRaises(ValueError):
            s.audit_evidence(records, support, 5, 60, [dict(inv[0], ignored=[])])

    def test_invalid_cli_arguments_fail_before_build(self):
        for args in (['--count', '1'], ['--shard', '-1'], ['--shard', '8'],
                     ['--build-bundles', 'a', '--from-bundle', 'b'], ['--from-bundle', 'b', '--inventory-only'],
                     ['--audit', 'a', '--build-bundles', 'b']):
            proc = subprocess.run(['python3', str(ROOT/'scripts/ci-shards.py'), '--output', '/tmp/unused-man498.json', *args], capture_output=True)
            self.assertEqual(proc.returncode, 2)

    def test_real_cargo_fixture(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp)
            (p/'src').mkdir(); (p/'tests').mkdir(); (p/'data').write_text('fixture')
            (p/'Cargo.toml').write_text('[package]\nname="shard-fixture"\nversion="0.1.0"\nedition="2021"\n[features]\nextra=[]\n[[test]]\nname="excluded"\nrequired-features=["extra"]\n')
            (p/'src/lib.rs').write_text('/// ```\n/// assert_eq!(2+2,4);\n/// ```\npub fn f() {}\n#[test] fn unit() {}\n#[test] #[ignore] fn ignored() { panic!("ignored"); }\n')
            (p/'src/main.rs').write_text('fn main() { println!("fixture"); }')
            (p/'tests/excluded.rs').write_text('#[test] fn excluded() { panic!(); }')
            (p/'tests/integration.rs').write_text('#[test] fn integration() { assert_eq!(std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap(), "0"); assert_eq!(std::env::var("CARGO_PKG_NAME").unwrap(), "shard-fixture"); assert_eq!(std::fs::read_to_string("data").unwrap(), "fixture"); assert!(std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("data").exists()); assert!(std::process::Command::new(env!("CARGO_BIN_EXE_shard-fixture")).status().unwrap().success()); }')
            subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=p, check=True, capture_output=True)
            records = []
            output = []
            for i in range(2):
                dest = p/f'{i}.json'
                proc = subprocess.run(['python3', str(ROOT/'scripts/ci-shards.py'), '--count', '2', '--shard', str(i), '--output', str(dest)], cwd=p, capture_output=True, text=True)
                self.assertEqual(proc.returncode, 0, proc.stdout+proc.stderr)
                records.append(json.loads(dest.read_text()))
                output.append(proc.stdout)
            self.assertEqual(sum(len(r['selected']) for r in records), 3)
            self.assertTrue(any('ignored' in x['ignored'] for x in records[0]['inventory']))
            self.assertFalse(any(x['name'] == 'excluded' for x in records[0]['inventory']))
            self.assertIn('1 ignored', ''.join(output))
            for record in records:
                for result in record['results']:
                    self.assertGreaterEqual(result['seconds'], 0)
                    # cargo metadata reports resolved paths; macOS tmp dirs are
                    # /var -> /private/var symlinks, so compare resolved paths.
                    self.assertEqual(Path(result['cwd']).resolve(), p.resolve())
                    self.assertEqual(len(result['command']), 1)
            doc = subprocess.run(['cargo', 'test', '--doc', '--locked'], cwd=p, check=True, capture_output=True, text=True)
            self.assertIn('1 passed', doc.stdout)
            (p/'tests/integration.rs').write_text('#[test] fn fails() { panic!("failure propagation"); }')
            failures = [subprocess.run(['python3', str(ROOT/'scripts/ci-shards.py'), '--count', '2', '--shard', str(i), '--output', str(p/f'fail{i}.json')], cwd=p, capture_output=True).returncode for i in range(2)]
            self.assertIn(1, failures)
            self.assertTrue(any(x['exitCode'] != 0 for i in range(2) for x in json.loads((p/f'fail{i}.json').read_text())['results']))
            (p/'src/main.rs').write_text('not rust')
            proc = subprocess.run(['python3', str(ROOT/'scripts/ci-shards.py'), '--output', str(p/'broken.json')], cwd=p, capture_output=True)
            self.assertNotEqual(proc.returncode, 0)

    def test_env_from_a_plan_matches_cargos_env(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp)/'target'
            (target/'debug/build/x/out').mkdir(parents=True)
            rustlib = str(Path(tmp)/'sysroot/lib/rustlib/x86_64-unknown-linux-gnu/lib')
            pkg = dict(manifest_path=str(Path(tmp)/'Cargo.toml'), name='p', version='1.2.3-rc.1+meta',
                       authors=['a', 'b'], description=None, license='MIT')
            cargo = shutil.which('cargo') or 'cargo'
            variables, paths = json.loads(json.dumps(s.cargo_env(pkg, target, rustlib, cargo)))
            self.assertEqual(s.compose_env(os.environ, variables, paths), s.child_env(pkg, target, rustlib))
            self.assertEqual((variables['CARGO_PKG_VERSION_PRE'], variables['CARGO_PKG_AUTHORS']), ('rc.1', 'a:b'))
            self.assertIn(str(target/'debug/build/x/out'), paths)
            name = 'DYLD_FALLBACK_LIBRARY_PATH' if sys.platform == 'darwin' else 'LD_LIBRARY_PATH'
            env = s.compose_env({name: '/existing', 'KEEP': '1'}, variables, paths)
            self.assertEqual(env[name].split(os.pathsep), paths + ['/existing'])
            self.assertEqual(env['KEEP'], '1')

    def test_bundle_members_must_be_executables_under_target(self):
        def bundle(path, *members):
            with tarfile.open(path, 'w:gz') as tar:
                plan = b'{}'
                info = tarfile.TarInfo(s.PLAN_NAME); info.size = len(plan)
                tar.addfile(info, io.BytesIO(plan))
                for info, data in members:
                    tar.addfile(info, io.BytesIO(data) if data is not None else None)
            return path

        def member(name, kind=tarfile.REGTYPE, data=b'x', link=''):
            info = tarfile.TarInfo(name); info.type = kind; info.mode = 0o755; info.linkname = link
            info.size = len(data) if kind == tarfile.REGTYPE else 0
            return info, (data if kind == tarfile.REGTYPE else None)

        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp)
            good = bundle(p/'good.tar.gz', member('target/debug/deps/x', data=b'\x7fELF'))
            with tarfile.open(good) as tar:
                self.assertEqual(s.read_plan(tar), {})
                s.extract_members(tar, s.safe_members(tar, 'target'), p/'root')
            self.assertEqual((p/'root/target/debug/deps/x').read_bytes(), b'\x7fELF')
            self.assertEqual((p/'root/target/debug/deps/x').stat().st_mode & 0o777, 0o755)
            for i, bad in enumerate([member('../evil'), member('/abs'), member('target/../evil'),
                                     member('target/debug/link', tarfile.SYMTYPE, link='/etc/passwd'),
                                     member('target/debug/hard', tarfile.LNKTYPE, link='target/debug/deps/x'),
                                     member('src/lib.rs'), member('src', tarfile.DIRTYPE),
                                     member('targetx/debug/x')]):
                dest = p/f'bad{i}'
                with tarfile.open(bundle(p/f'bad{i}.tar.gz', member('target/debug/deps/ok'), bad)) as tar:
                    s.read_plan(tar)
                    with self.assertRaises(ValueError):
                        s.safe_members(tar, 'target')
                self.assertFalse(dest.exists())
            with tarfile.open(bundle(p/'noplan.tar.gz')) as tar:
                tar.next()
                with self.assertRaises(ValueError):
                    s.read_plan(tar)

    def test_run_mode_refuses_a_foreign_bundle(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp).resolve()
            (p/'README').write_text('x')
            git(p, 'init', '-q'); git(p, 'add', '-A'); git(p, 'commit', '-q', '-m', 'one')
            head = git(p, 'rev-parse', 'HEAD')
            (p/'bin').write_text('#!/bin/sh\nexit 0\n')
            plan = dict(version=1, index=0, count=8, workspaceRoot=str(p), targetDirectory=str(p/'target'),
                        gitHead=head, gitTree='t', inventory=[], selected=[])
            cases = [(['--shard', '1'], {}, 'shard 0 of 8'), (['--count', '4'], {}, 'shard 0 of 8'),
                     ([], dict(workspaceRoot=str(p/'elsewhere')), 'built in'), ([], dict(gitHead='0' * 40), 'shard HEAD is')]
            for extra, change, cause in cases:
                bundle = p/'slice.tar.gz'
                files = [str(p/'bin')]
                # A real slice only carries files under target/; this one is never extracted.
                s.write_slice(bundle, dict(plan, **change), p, files)
                out = p/'out.json'
                argv = ['--count', '8', '--shard', '0', *extra, '--from-bundle', str(bundle), '--output', str(out)]
                proc = subprocess.run([sys.executable, SCRIPT, *argv], cwd=p, capture_output=True, text=True)
                self.assertEqual(proc.returncode, 1, proc.stderr)
                self.assertIn(cause, json.loads(out.read_text())['error'])
                self.assertFalse((p/'target').exists())

    def test_run_mode_blocks_compilers(self):
        with tempfile.TemporaryDirectory() as tmp:
            tripwire = s.make_tripwire(tmp)
            item = dict(env=dict(CARGO_PKG_NAME='p', CARGO='/real/cargo'), libraryPath=['/lib/x'])
            env = s.run_env(item, tripwire)
            self.assertEqual(env['CARGO'], str(Path(tmp)/'cargo'))
            self.assertEqual(env['CARGO_PKG_NAME'], 'p')
            for tool in ('cargo', 'rustc', 'rustup'):
                self.assertEqual(shutil.which(tool, path=env['PATH']), str(Path(tmp)/tool))
                proc = subprocess.run([tool, '--version'], env=env, capture_output=True, text=True)
                self.assertEqual(proc.returncode, 97)
                self.assertIn('MAN-518 tripwire', proc.stderr)

    def test_audit_rejects_mixed_commits_and_build_drift(self):
        inv = [dict(key='a', tests=['ok'], ignored=[])]
        support = dict(conclusion='success', doctests='success', started=10, completed=50)
        def shards(*heads):
            return [dict(index=i, count=2, inventory=inv, selected=inv if i == 0 else [], gitHead=h,
                         results=[dict(key='a', exitCode=0)] if i == 0 else []) for i, h in enumerate(heads)]
        build = dict(inventory=inv, gitHead='h')
        self.assertEqual(s.audit_evidence(shards('h', 'h'), support, 5, 60, None, build), 55)
        for records, b in [(shards('h', 'g'), None), (shards('h', None), None),
                           (shards('h', 'h'), dict(build, gitHead='g')),
                           (shards('h', 'h'), dict(build, inventory=[dict(inv[0], tests=[])]))]:
            with self.assertRaises(ValueError):
                s.audit_evidence(records, support, 5, 60, None, b)
        legacy = [{k: v for k, v in r.items() if k != 'gitHead'} for r in shards('h', 'h')]
        self.assertEqual(s.audit_evidence(legacy, support, 5, 60), 55)

    def test_real_cargo_fixture_bundles_match_legacy(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp).resolve()/'ws'
            p.mkdir()
            fixture_crate(p)
            (p/'.gitignore').write_text('/target\n/bundles\n*.json\n')
            git(p, 'init', '-q'); git(p, 'add', '-A'); git(p, 'commit', '-q', '-m', 'fixture')
            nocc = no_compiler_path(Path(tmp)/'nocc')
            self.assertIsNone(shutil.which('cargo', path=nocc))
            self.assertIsNone(shutil.which('rustc', path=nocc))

            def run(*argv, path=None):
                env = dict(os.environ, PATH=os.pathsep.join([path, '/usr/bin', '/bin'])) if path else None
                if path:
                    for directory in ('/usr/bin', '/bin'):
                        for tool in ('cargo', 'rustc', 'rustup'):
                            self.assertFalse(os.path.exists(os.path.join(directory, tool)), f'{tool} in {directory}')
                return subprocess.run([sys.executable, SCRIPT, *argv], cwd=p, env=env, capture_output=True, text=True)

            def both_modes(tag):
                legacy = []
                for i in range(2):
                    proc = run('--count', '2', '--shard', str(i), '--output', str(p/f'{tag}-legacy-{i}.json'))
                    legacy.append((proc.returncode, json.loads((p/f'{tag}-legacy-{i}.json').read_text())))
                proc = run('--count', '2', '--build-bundles', 'bundles', '--output', str(p/f'{tag}-build.json'))
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                build = json.loads((p/f'{tag}-build.json').read_text())
                shutil.move(str(p/'target'), str(Path(tmp)/f'{tag}-target'))
                bundled = []
                for i in range(2):
                    proc = run('--count', '2', '--shard', str(i), '--from-bundle', f'bundles/slice-{i}.tar.gz',
                               '--output', str(p/f'{tag}-bundle-{i}.json'), path=nocc)
                    self.assertNotIn('MAN-518 tripwire', proc.stderr)
                    bundled.append((proc.returncode, json.loads((p/f'{tag}-bundle-{i}.json').read_text())))
                return legacy, build, bundled

            legacy, build, bundled = both_modes('pass')
            head = git(p, 'rev-parse', 'HEAD')
            self.assertEqual([code for code, _ in legacy + bundled], [0, 0, 0, 0], bundled)
            self.assertEqual(canonical(build['inventory']), canonical(legacy[0][1]['inventory']))
            for code, record in bundled:
                self.assertNotIn('error', record)
                self.assertEqual(canonical(record['inventory']), canonical(legacy[0][1]['inventory']))
                self.assertEqual(record['buildSeconds'], 0)
                self.assertGreaterEqual(record['extractSeconds'], 0)
                self.assertEqual(record['gitHead'], head)
            self.assertEqual(build['gitHead'], head)
            self.assertEqual(sorted(x['key'] for _, r in bundled for x in r['selected']),
                             sorted(x['key'] for _, r in legacy for x in r['selected']))
            verdicts = lambda runs: {x['key']: x['exitCode'] for _, r in runs for x in r['results']}
            self.assertEqual(verdicts(bundled), verdicts(legacy))
            self.assertEqual(len(verdicts(bundled)), 3)
            self.assertTrue((p/'target/tmp').is_dir())
            fixture_bin = p/'target/debug/shard-fixture'
            self.assertTrue(os.access(fixture_bin, os.X_OK))
            self.assertEqual(s.audit_evidence([r for _, r in bundled], dict(conclusion='success', doctests='success', started=1, completed=2),
                                              0, 3, legacy[0][1]['inventory'], build), 3)

            (p/'tests/integration.rs').write_text('#[test] fn fails() { panic!("failure propagation"); }')
            git(p, 'commit', '-q', '-am', 'fail')
            shutil.rmtree(p/'target')
            shutil.rmtree(p/'bundles')
            legacy, build, bundled = both_modes('fail')
            for runs in (legacy, bundled):
                failing = [x for _, r in runs for x in r['results'] if x['exitCode'] != 0]
                self.assertEqual([x['key'].rsplit('::', 1)[1] for x in failing], ['integration'])
                holder = [code for code, r in runs if any(x['exitCode'] for x in r['results'])]
                self.assertEqual(holder, [1])
            self.assertEqual(verdicts(bundled), verdicts(legacy))

    def test_bundle_build_failure_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp).resolve()
            fixture_crate(p)
            git(p, 'init', '-q'); git(p, 'add', '-A'); git(p, 'commit', '-q', '-m', 'fixture')
            (p/'src/main.rs').write_text('not rust')
            out = p/'build.json'
            proc = subprocess.run([sys.executable, SCRIPT, '--count', '2', '--build-bundles', 'bundles', '--output', str(out)],
                                  cwd=p, capture_output=True, text=True)
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn('error', json.loads(out.read_text()))
            self.assertEqual(list(p.glob('bundles/slice-*.tar.gz')), [])


if __name__ == '__main__':
    unittest.main()
