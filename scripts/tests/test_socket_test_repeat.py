"""MAN-523: changed socket tests run ten more times and every run must pass (stdlib, real Cargo fixture)."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest

ROOT = Path(__file__).resolve().parents[2]


def load(name, file):
    spec = importlib.util.spec_from_file_location(name, ROOT/'scripts'/file)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


r = load('socket_test_repeat', 'socket-test-repeat.py')
shards = load('ci_shards_for_repeat', 'ci-shards.py')
SHARDS = str(ROOT/'scripts/ci-shards.py')


def git(cwd, *args):
    env = dict(os.environ, GIT_CONFIG_COUNT='0', GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull, HOME=str(cwd))
    for key in ('GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE'):
        env.pop(key, None)
    return subprocess.run(['git', '-c', 'user.name=MAN-523', '-c', 'user.email=man523@example.invalid',
                           '-c', 'commit.gpgsign=false', *args], cwd=cwd, env=env,
                          check=True, capture_output=True, text=True).stdout.strip()


SOCKET_TEST = '''mod common;
use std::net::TcpListener;

fn helper() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
fn first() {
    let text = "a { brace } in a string";
    assert!(helper() > 0 && !text.is_empty());
}

#[tokio::test(
    flavor = "multi_thread",
)]
async fn second() {
    // a } in a comment
    assert_eq!(1, 1);
}
'''
COMMON = 'pub fn shared() -> u32 { 1 }\n'
UNIT = '''pub fn production() -> u32 { 1 }

#[cfg(test)]
mod tests {
    use std::net::UdpSocket;

    fn claim() -> UdpSocket {
        UdpSocket::bind("127.0.0.1:0").unwrap()
    }

    #[test]
    fn unit_one() {
        let _ = claim();
    }

    #[test]
    fn unit_two() {}
}
'''


class Repo:
    """A git repository holding one package under crates/p."""

    def __init__(self, tmp):
        self.root = Path(tmp)
        pkg = self.root/'crates/p'
        (pkg/'tests/common').mkdir(parents=True)
        (pkg/'src').mkdir()
        (pkg/'tests/sock.rs').write_text(SOCKET_TEST)
        (pkg/'tests/common/mod.rs').write_text(COMMON)
        (pkg/'tests/plain.rs').write_text('#[test]\nfn plain() {}\n')
        (pkg/'src/net.rs').write_text(UNIT)
        git(self.root, 'init', '-q')
        git(self.root, 'add', '-A')
        git(self.root, 'commit', '-q', '-m', 'base')
        self.base = git(self.root, 'rev-parse', 'HEAD')

    def edit(self, rel, old, new):
        path = self.root/rel
        text = path.read_text()
        assert old in text, old
        path.write_text(text.replace(old, new))
        git(self.root, 'commit', '-qam', 'edit')

    def items(self):
        return [
            dict(key='p::test::sock', kind='test', name='sock', cwd='crates/p', tests=['first', 'second']),
            dict(key='p::test::plain', kind='test', name='plain', cwd='crates/p', tests=['plain']),
            dict(key='p::lib::p', kind='lib', name='p', cwd='crates/p',
                 tests=['net::tests::unit_one', 'net::tests::unit_two']),
        ]

    def plan(self):
        return r.plan(self.root, self.items(), self.base)

    def selected(self):
        return sorted((x['key'], x['test']) for x in self.plan()['selected'])


class Parsing(unittest.TestCase):
    def test_test_functions_ignore_braces_in_strings_and_comments_and_span_multiline_attributes(self):
        spans = {name: (first, last) for name, first, last in r.test_functions(SOCKET_TEST)}
        self.assertEqual(set(spans), {'first', 'second'})
        lines = SOCKET_TEST.splitlines()
        self.assertEqual(lines[spans['first'][0]-1], '#[test]')
        self.assertEqual(lines[spans['first'][1]-1], '}')
        self.assertEqual(lines[spans['second'][0]-1], '#[tokio::test(')
        self.assertEqual(spans['second'][1], len(lines))

    def test_raw_strings_and_char_literals_do_not_open_blocks(self):
        text = '#[test]\nfn raw() {\n    let _ = r#"{ " }"#;\n    let _ = \'{\';\n}\n#[test]\nfn after() {}\n'
        self.assertEqual([(n, a, b) for n, a, b in r.test_functions(text)], [('raw', 1, 5), ('after', 6, 7)])

    def test_module_prefix(self):
        src = Path('/x/src')
        self.assertEqual(r.module_prefix(src, src/'list_http.rs'), 'list_http::')
        self.assertEqual(r.module_prefix(src, src/'a/mod.rs'), 'a::')
        self.assertEqual(r.module_prefix(src, src/'a/b.rs'), 'a::b::')
        self.assertEqual(r.module_prefix(src, src/'main.rs'), '')


class Selection(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='man523-')
        self.repo = Repo(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def test_an_edited_socket_test_selects_only_that_test(self):
        self.repo.edit('crates/p/tests/sock.rs', 'assert_eq!(1, 1);', 'assert_eq!(2, 2);')
        self.assertEqual(self.repo.selected(), [('p::test::sock', 'second')])

    def test_an_added_socket_test_is_selected(self):
        self.repo.edit('crates/p/tests/sock.rs', '#[test]\nfn first()', '#[test]\nfn added() {}\n\n#[test]\nfn first()')
        items = self.repo.items()
        items[0]['tests'].append('added')
        self.assertEqual([x['test'] for x in r.plan(self.repo.root, items, self.repo.base)['selected']], ['added'])

    def test_a_changed_helper_in_the_target_selects_every_test_of_the_target(self):
        self.repo.edit('crates/p/tests/sock.rs', 'listener.local_addr().unwrap().port()', 'listener.local_addr().unwrap().port() + 0')
        self.assertEqual(self.repo.selected(), [('p::test::sock', 'first'), ('p::test::sock', 'second')])

    def test_a_shared_helper_selects_the_socket_targets_that_include_it(self):
        self.repo.edit('crates/p/tests/common/mod.rs', '{ 1 }', '{ 2 }')
        self.assertEqual(self.repo.selected(), [('p::test::sock', 'first'), ('p::test::sock', 'second')])

    def test_a_target_without_a_socket_bind_is_not_repeated(self):
        self.repo.edit('crates/p/tests/plain.rs', 'fn plain() {}', 'fn plain() { assert!(true); }')
        plan = self.repo.plan()
        self.assertEqual(plan['selected'], [])
        self.assertEqual(plan['noop'], 'no relevant change')

    def test_a_unit_test_edit_selects_that_test_and_production_code_selects_nothing(self):
        self.repo.edit('crates/p/src/net.rs', 'fn unit_two() {}', 'fn unit_two() { let _ = 1; }')
        self.assertEqual(self.repo.selected(), [('p::lib::p', 'net::tests::unit_two')])
        self.repo.base = git(self.repo.root, 'rev-parse', 'HEAD')
        self.repo.edit('crates/p/src/net.rs', 'production() -> u32 { 1 }', 'production() -> u32 { 2 }')
        self.assertEqual(self.repo.selected(), [])

    def test_a_unit_test_helper_selects_every_test_of_its_module(self):
        self.repo.edit('crates/p/src/net.rs', 'UdpSocket::bind("127.0.0.1:0").unwrap()', 'UdpSocket::bind("127.0.0.1:0").expect("claim")')
        self.assertEqual(self.repo.selected(), [('p::lib::p', 'net::tests::unit_one'), ('p::lib::p', 'net::tests::unit_two')])

    def test_the_recording_seed_is_repeated_without_a_socket(self):
        tests = self.repo.root/'crates/manta-input/tests'
        tests.mkdir(parents=True)
        (tests/'recording.rs').write_text('#[test]\nfn a_recorded_source_reads_exactly() {}\n')
        git(self.repo.root, 'add', '-A')
        git(self.repo.root, 'commit', '-qm', 'seed')
        item = dict(key='mi::test::recording', kind='test', name='recording', cwd='crates/manta-input',
                    tests=['a_recorded_source_reads_exactly'])
        plan = r.plan(self.repo.root, [item], self.repo.base)
        self.assertEqual([x['test'] for x in plan['selected']], ['a_recorded_source_reads_exactly'])

    def test_a_deleted_target_is_reported_removed(self):
        git(self.repo.root, 'rm', '-q', 'crates/p/tests/plain.rs')
        git(self.repo.root, 'commit', '-qm', 'rm')
        plan = self.repo.plan()
        self.assertEqual(plan['removed'], ['crates/p/tests/plain.rs'])
        self.assertEqual(plan['selected'], [])

    def test_no_rust_change_is_an_explicit_noop(self):
        (self.repo.root/'README').write_text('docs')
        git(self.repo.root, 'add', '-A')
        git(self.repo.root, 'commit', '-qm', 'docs')
        self.assertEqual(self.repo.plan()['noop'], 'no Rust change')

    def test_an_ignored_test_is_never_selected(self):
        self.repo.edit('crates/p/tests/sock.rs', 'assert_eq!(1, 1);', 'assert_eq!(2, 2);')
        items = self.repo.items()
        items[0]['ignored'] = ['second']
        self.assertEqual(r.plan(self.repo.root, items, self.repo.base)['selected'], [])

    def test_a_deletion_under_a_blank_line_still_selects_its_test(self):
        self.repo.edit('crates/p/tests/sock.rs', '    // a } in a comment\n    assert_eq!(1, 1);\n',
                       '    // a } in a comment\n\n    assert_eq!(1, 1);\n')
        self.repo.base = git(self.repo.root, 'rev-parse', 'HEAD')
        self.repo.edit('crates/p/tests/sock.rs', '\n\n    assert_eq!(1, 1);\n', '\n\n')
        self.assertEqual(self.repo.selected(), [('p::test::sock', 'second')])

    def test_a_comment_only_helper_edit_selects_nothing(self):
        self.repo.edit('crates/p/tests/common/mod.rs', 'pub fn shared', '// a note\npub fn shared')
        self.assertEqual(self.repo.selected(), [])

    def test_hunk_lines_that_look_like_file_headers_do_not_reset_the_file(self):
        self.repo.edit('crates/p/tests/sock.rs', 'let text = "a { brace } in a string";',
                       'let text = "a { brace } in a string\n++ b/elsewhere.rs\n";')
        self.assertEqual(self.repo.selected(), [('p::test::sock', 'first')])

    def test_a_whole_file_test_module_selects_the_modules_that_name_it(self):
        src = self.repo.root/'crates/p/src'
        (src/'lib.rs').write_text('pub mod net;\npub mod other;\n#[cfg(test)]\nmod ports;\n')
        (src/'ports.rs').write_text('pub(crate) fn port() -> u16 { 1 }\n')
        (src/'other.rs').write_text('#[cfg(test)]\nmod tests {\n    #[test]\n    fn other_one() {}\n}\n')
        git(self.repo.root, 'add', '-A')
        self.repo.edit('crates/p/src/net.rs', 'fn unit_two() {}', 'fn unit_two() { crate::ports::port(); }')
        self.repo.base = git(self.repo.root, 'rev-parse', 'HEAD')
        self.repo.edit('crates/p/src/ports.rs', '{ 1 }', '{ 2 }')
        items = self.repo.items()
        items[2]['tests'].append('other::tests::other_one')
        picked = sorted(x['test'] for x in r.plan(self.repo.root, items, self.repo.base)['selected'])
        self.assertEqual(picked, ['net::tests::unit_one', 'net::tests::unit_two'])

    def test_head_first_parent_is_a_valid_base(self):
        self.repo.edit('crates/p/tests/sock.rs', 'assert_eq!(1, 1);', 'assert_eq!(2, 2);')
        plan = r.plan(self.repo.root, self.repo.items(), 'HEAD^1')
        self.assertEqual(plan['base'], self.repo.base)
        self.assertEqual([x['test'] for x in plan['selected']], ['second'])

    def test_a_missing_or_zero_base_is_an_error_not_an_empty_diff(self):
        for base in ('', '0' * 40, 'not-a-sha'):
            with self.assertRaises(ValueError):
                r.plan(self.repo.root, self.repo.items(), base)
        with self.assertRaises(ValueError):
            r.plan(self.repo.root, self.repo.items(), 'f' * 40)


def completed(code, passed):
    return types.SimpleNamespace(returncode=code, stdout=f'running 1 test\ntest result: {"ok" if code == 0 else "FAILED"}. {passed} passed; 0 failed;\n')


class Repetition(unittest.TestCase):
    ITEM = dict(key='k', executable='/bin/false')

    def test_ten_passes_are_required_and_recorded(self):
        calls = []
        result = r.repeat(self.ITEM, 'name', '/', {}, runner=lambda cmd, **kw: calls.append(cmd) or completed(0, 1))
        self.assertTrue(result['ok'])
        self.assertEqual(result['passes'], 10)
        self.assertEqual(calls, [['/bin/false', '--exact', 'name']] * 10)

    def test_a_failure_on_iteration_seven_fails_and_is_never_retried(self):
        outcomes = [completed(0, 1)] * 6 + [completed(101, 0)] + [completed(0, 1)] * 10
        calls = []

        def runner(cmd, **kw):
            calls.append(cmd)
            return outcomes[len(calls) - 1]
        result = r.repeat(self.ITEM, 'name', '/', {}, runner=runner)
        self.assertFalse(result['ok'])
        self.assertEqual(result['passes'], 6)
        self.assertEqual(len(calls), 7)
        with self.assertRaises(ValueError):
            r.audit_repeats(dict(selected=[dict(key='k', test='name')]), [result])

    def test_zero_tests_run_is_not_a_pass(self):
        result = r.repeat(self.ITEM, 'gone', '/', {}, runner=lambda cmd, **kw: completed(0, 0))
        self.assertFalse(result['ok'])

    def test_fewer_than_ten_repeats_is_refused(self):
        with self.assertRaises(ValueError):
            r.repeat(self.ITEM, 'name', '/', {}, count=9, runner=lambda cmd, **kw: completed(0, 1))

    def test_audit_rejects_missing_and_duplicate_results(self):
        ok = dict(key='k', test='a', count=10, passes=10, ok=True)
        r.audit_repeats(dict(selected=[dict(key='k', test='a')]), [ok])
        with self.assertRaises(ValueError):
            r.audit_repeats(dict(selected=[dict(key='k', test='a'), dict(key='k', test='b')]), [ok])
        with self.assertRaises(ValueError):
            r.audit_repeats(dict(selected=[dict(key='k', test='a')]), [ok, ok])


class ShardWiring(unittest.TestCase):
    def test_repeat_base_needs_from_bundle(self):
        with tempfile.TemporaryDirectory() as tmp:
            proc = subprocess.run([sys.executable, SHARDS, '--count', '2', '--repeat-base', 'a' * 40,
                                   '--output', str(Path(tmp)/'o.json')], capture_output=True, text=True)
        self.assertEqual(proc.returncode, 2)
        self.assertIn('--repeat-base needs --from-bundle or --inventory-only', proc.stderr)

    def test_a_noop_plan_runs_nothing_and_a_failed_ordinary_run_skips_repeats(self):
        args = types.SimpleNamespace(repeat_base='a' * 40)
        fake = types.SimpleNamespace(plan=lambda root, items, base: dict(selected=[], noop='no relevant change'),
                                     repeat=lambda *a, **k: self.fail('repeated on a no-op'))
        original = shards.load_repeater
        shards.load_repeater = lambda: fake
        try:
            record = dict(results=[dict(exitCode=0)])
            self.assertEqual(shards.run_repeats(args, record, Path('.'), []), 0)
            self.assertEqual(record['repeatResults'], [])
            fake.plan = lambda root, items, base: dict(selected=[dict(key='k', test='t', reason='x')], noop=None)
            record = dict(results=[dict(exitCode=101)])
            self.assertEqual(shards.run_repeats(args, record, Path('.'), [(dict(key='k'), '.', {})]), 1)
            self.assertEqual(record['repeatSkipped'], 'an ordinary run failed')
        finally:
            shards.load_repeater = original


FIXTURE_TOML = '[package]\nname="repeat-fixture"\nversion="0.1.0"\nedition="2021"\n'
FIXTURE_TEST = '''use std::net::TcpListener;

#[test]
fn held() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    assert!(listener.local_addr().unwrap().port() > 0);
}

#[test]
fn sibling() {
    let _ = std::fs::OpenOptions::new().append(true).create(true).open("sibling-runs").unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open("sibling-runs").unwrap().write_all(b"x").unwrap();
}

#[test]
fn flaky() {
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).create(true).open("flaky-runs").unwrap().write_all(b"x").unwrap();
    let runs = std::fs::read("flaky-runs").unwrap().len();
    // The ordinary run is 1; repeat iteration seven is run 8.
    assert!(std::env::var_os("FLAKY").is_none() || runs != 8, "run {runs}");
}
'''


class RealCargo(unittest.TestCase):
    """A real bundle: the ordinary run once, then ten fresh `--exact` runs of the edited test."""

    def run_fixture(self, flaky, inventory_only=False):
        with tempfile.TemporaryDirectory(prefix='man523-cargo-') as tmp:
            p = Path(tmp)/'w'
            (p/'src').mkdir(parents=True)
            (p/'tests').mkdir()
            (p/'Cargo.toml').write_text(FIXTURE_TOML)
            (p/'src/lib.rs').write_text('pub fn f() {}\n')
            (p/'tests/sock.rs').write_text(FIXTURE_TEST)
            (p/'.gitignore').write_text('target/\nbundles/\n*.json\n*-runs\nCargo.lock\n')
            subprocess.run(['cargo', 'generate-lockfile', '--offline'], cwd=p, check=True, capture_output=True)
            git(p, 'init', '-q')
            git(p, 'add', '-A')
            git(p, 'commit', '-qm', 'base')
            base = git(p, 'rev-parse', 'HEAD')
            edited = 'run 8' if not flaky else 'run {runs} of the flaky test'
            (p/'tests/sock.rs').write_text(FIXTURE_TEST.replace('"run {runs}"', f'"{edited}"'))
            git(p, 'commit', '-qam', 'edit flaky')
            env = dict(os.environ)
            env.pop('FLAKY', None)
            if flaky:
                env['FLAKY'] = '1'
            if inventory_only:
                # The Windows job: its ordinary `cargo test --workspace` already ran.
                subprocess.run(['cargo', 'test', '--locked'], cwd=p, env=env, check=True, capture_output=True)
                out = p/'windows.json'
                proc = subprocess.run([sys.executable, SHARDS, '--count', '2', '--shard', '0', '--inventory-only',
                                       '--repeat-base', base, '--output', str(out)], cwd=p, env=env,
                                      capture_output=True, text=True)
                return [(proc.returncode, json.loads(out.read_text()))], len((p/'sibling-runs').read_bytes())
            proc = subprocess.run([sys.executable, SHARDS, '--count', '2', '--build-bundles', 'bundles',
                                   '--output', str(p/'build.json')], cwd=p, env=env, capture_output=True, text=True)
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            records = []
            for i in range(2):
                out = p/f'shard-{i}.json'
                proc = subprocess.run([sys.executable, SHARDS, '--count', '2', '--shard', str(i),
                                       '--from-bundle', f'bundles/slice-{i}.tar.gz', '--repeat-base', base,
                                       '--output', str(out)], cwd=p, env=env, capture_output=True, text=True)
                self.assertNotIn('MAN-518 tripwire', proc.stderr)
                records.append((proc.returncode, json.loads(out.read_text())))
            sibling_runs = len((p/'sibling-runs').read_bytes())
            return records, sibling_runs

    def test_the_edited_socket_test_runs_ten_more_times_and_siblings_once(self):
        records, sibling_runs = self.run_fixture(flaky=False)
        self.assertEqual([code for code, _ in records], [0, 0], records)
        repeats = [x for _, rec in records for x in rec.get('repeatResults', [])]
        self.assertEqual([(x['test'], x['passes'], x['ok']) for x in repeats], [('flaky', 10, True)])
        self.assertEqual(sum(len(rec['results']) for _, rec in records), 2)
        self.assertEqual(sibling_runs, 1)

    def test_a_failure_on_iteration_seven_fails_the_shard(self):
        records, _ = self.run_fixture(flaky=True)
        failed = [(code, rec) for code, rec in records if rec.get('repeatResults')]
        self.assertEqual(len(failed), 1, records)
        code, rec = failed[0]
        self.assertEqual(code, 1)
        self.assertEqual(rec['repeatResults'][0]['passes'], 6)
        self.assertEqual(all(x['exitCode'] == 0 for x in rec['results']), True)

    def test_inventory_only_repeats_without_rerunning_the_suite(self):
        records, sibling_runs = self.run_fixture(flaky=False, inventory_only=True)
        (code, record), = records
        self.assertEqual(code, 0, record)
        self.assertEqual(record['results'], [])
        self.assertEqual([(x['test'], x['passes']) for x in record['repeatResults']], [('flaky', 10)])
        self.assertEqual(sibling_runs, 1)
        records, _ = self.run_fixture(flaky=True, inventory_only=True)
        (code, record), = records
        self.assertEqual(code, 1)
        self.assertEqual(record['repeatResults'][0]['passes'], 6)


if __name__ == '__main__':
    unittest.main()
