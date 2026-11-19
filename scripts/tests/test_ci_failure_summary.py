"""Contracts for Windows failure classification and job summaries."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('ci_failure_summary', ROOT/'scripts/ci-failure-summary.py')
s = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(s)


class ClassifyTests(unittest.TestCase):
    def test_historical_failures_classify_as_audited(self):
        cases = [
            ('health_command', 'health_escapes_peer_text', '', 'port reuse'),
            ('live_state_acceptance', 'the_live_listener_admits_at_most_32_clients', 'left: 0 right: 32', 'loopback timing'),
            ('other', 'socket', 'os error 10054', 'loopback timing'),
            ('config_command', 'cw_segments_documented_example_parses', '', 'CRLF'),
            ('recording', 'a_recorded_source_reads_exactly_like_the_bare_one_and_replays_its_trace', 'left: 100 right: 123', 'unknown'),
        ]
        for target, name, message, expected in cases:
            self.assertEqual(s.classify(target, name, message)[0], expected)

    def test_registry_wins_over_signatures(self):
        self.assertEqual(s.classify('health_command', 'health_escapes_peer_text', 'os error 10054')[0], 'port reuse')

    def test_unmatched_text_is_unknown(self):
        self.assertEqual(s.classify('x', 'y', 'attempt to add with overflow')[0], 'unknown')

    def test_registry_classes_are_the_three_known(self):
        self.assertEqual({x['class'] for x in s.REGISTRY['tests']}, {'port reuse', 'loopback timing', 'CRLF'})

    def test_registry_entries_are_in_the_man551_record(self):
        record = (ROOT/s.REGISTRY['record']).read_text(encoding='utf-8')
        for entry in s.REGISTRY['tests']:
            self.assertIn(entry['test'], record)


class RenderTests(unittest.TestCase):
    def test_failing_test_row_names_os_shard_binary_test_and_class(self):
        record = {'index': 2, 'results': [{'key': 'pkg::test::health_command', 'exitCode': 1,
                   'failedTests': ['health_escapes_peer_text'], 'failureExcerpts': {'health_escapes_peer_text': 'panic'}}]}
        out = s.render('windows-latest', [(Path('shard.json'), record)])
        for expected in ('Windows (windows-latest)', '| 2 | health_command | health_escapes_peer_text | port reuse |', '<details>'):
            self.assertIn(expected, out)

    def test_missing_shard_evidence_is_named(self):
        self.assertIn('shard 1: missing evidence', s.render('windows-latest', [], expect_shards=2))
        self.assertIn('shard 3: missing evidence', s.render('windows-latest', [], shard=3))

    def test_support_and_build_failures_are_named(self):
        out = s.render('windows-latest', [(Path('support'), {'doctests': 'failure', 'conclusion': 'failure'}),
                                          (Path('build'), {'role': 'build', 'error': 'failed'})])
        self.assertIn('support doctests: failure', out)
        self.assertIn('build: failed', out)

    def test_crash_and_tripwire_are_named(self):
        out = s.render('windows-latest', [(Path('shard'), {'index': 0, 'results': [
            {'key': 'pkg::test::crash', 'exitCode': 3221225477}, {'key': 'pkg::test::tripwire', 'exitCode': 97}]})])
        self.assertIn('0xC0000005', out)
        self.assertIn('tripwire exit 97', out)

    def test_repeat_failure_is_named(self):
        out = s.render('windows-latest', [(Path('shard'), {'index': 0, 'results': [],
                                                      'repeatResults': [{'ok': False, 'test': 'socket'}]})])
        self.assertIn('socket-test repeat failed: socket', out)

    def test_all_green_is_one_line(self):
        out = s.render('windows-latest', [(Path('shard'), {'index': 0, 'results': [{'exitCode': 0}]})])
        self.assertEqual(len(out.splitlines()), 1)
        self.assertIn('no failures', out)

    def test_no_evidence_exits_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)/'summary'
            self.assertEqual(s.main(['--os', 'windows-latest', '--append', str(output)]), 0)
            self.assertIn('no failures', output.read_text())
