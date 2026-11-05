"""Whole-binary CI scheduling contracts (stdlib, including a real Cargo fixture)."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('ci_shards', ROOT / 'scripts/ci-shards.py')
s = importlib.util.module_from_spec(spec)
spec.loader.exec_module(s)


def artifact(name='a', kind='test', package='p', executable='/tmp/a', test=True):
    return dict(reason='compiler-artifact', package_id=package,
                target=dict(name=name, kind=[kind]), profile=dict(test=test), executable=executable)


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

    def test_complete_disjoint_stable_and_golden_isolated(self):
        items = s.parse_artifacts('\n'.join(json.dumps(artifact(n)) for n in ['golden_v8_v8w', 'new', 'a', 'b']))
        shards = s.assign(items, {}, 8)
        self.assertEqual(shards, s.assign(list(reversed(items)), {}, 8))
        self.assertEqual([x['name'] for x in shards[0]], ['golden_v8_v8w'])
        self.assertEqual(sorted(x['key'] for group in shards for x in group), sorted(x['key'] for x in items))
        for count in [0, 1]:
            with self.assertRaises(ValueError):
                s.assign(items, {}, count)
        collision = s.parse_artifacts('\n'.join(json.dumps(artifact(package=p)) for p in ['p', 'q']))
        self.assertEqual(len(collision), 2)

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
        for args in (['--count', '1'], ['--shard', '-1'], ['--shard', '8']):
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
                    self.assertEqual(result['cwd'], str(p))
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


if __name__ == '__main__':
    unittest.main()
