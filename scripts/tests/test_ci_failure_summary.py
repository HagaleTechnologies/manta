"""Contracts for CI failure classification and job summaries."""
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

    def test_registry_fix_commits_carry_their_own_ticket(self):
        import subprocess
        for entry in s.REGISTRY['tests']:
            if entry['fix'] == 'pending':
                continue
            # CI checks out depth 1, where the pinned history is absent;
            # run the pairing check wherever the commits resolve.
            probe = subprocess.run(['git', 'cat-file', '-e', entry['fix']], cwd=ROOT,
                                   capture_output=True)
            if probe.returncode != 0:
                self.skipTest(f"{entry['fix']} is not in this clone")
            subject = subprocess.check_output(
                ['git', 'log', '--format=%s', '-1', entry['fix']], cwd=ROOT, text=True)
            self.assertIn(entry['ticket'], subject, f"{entry['fix']} is not {entry['ticket']}")


class RenderTests(unittest.TestCase):
    def test_failed_doctest_is_named_and_classified(self):
        output = 'test src/lib.rs - example (line 12) ... FAILED\n\nfailures:\n\n---- src/lib.rs - example (line 12) stdout ----\nthread panicked: os error 10048\n'
        failures, omitted, tail = s.parse_doctest_failures(output)
        self.assertEqual((omitted, tail), (0, ''))
        self.assertEqual([item['name'] for item in failures], ['src/lib.rs - example (line 12)'])
        summary = s.render('windows-latest', [(Path('support.json'), {'conclusion': 'failure', 'doctests': 'failure', 'doctestFailures': failures})])
        self.assertIn('| support | doctest | src/lib.rs - example (line 12) | port reuse |', summary)

    # A real `cargo test --doc --no-fail-fast` report (rustc 1.98, edition
    # 2024 merged doctests): the port-reuse doctest prints 300 lines before
    # the AddrInUse panic. A head-only excerpt cut it off and classified the
    # failure `unknown` (MAN-551 validate).
    NOISY = ('   Doc-tests manta_a\n\nrunning 2 tests\n'
             'test src/lib.rs - (line 8) ... FAILED\ntest src/lib.rs - (line 1) ... FAILED\n\nfailures:\n\n'
             '---- src/lib.rs - (line 1) stdout ----\nTest executable failed (exit code: 101).\n\nstdout:\n'
             + ''.join(f'noise line {i} xxxxxxxxxxxxxxxxxxxxxxxx\n' for i in range(300)) +
             "\nstderr:\n\nthread 'main' (41065276) panicked at doctest_bundle_2024.rs:9:32:\n"
             'called `Result::unwrap()` on an `Err` value: Os { code: 48, kind: AddrInUse, message: "Address already in use" }\n'
             'note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\n'
             '---- src/lib.rs - (line 8) stdout ----\n\nthread \'main\' panicked at src/lib.rs:9:1:\n'
             'assertion `left == right` failed\n  left: 1\n right: 2\n\n'
             'failures:\n    src/lib.rs - (line 1)\n    src/lib.rs - (line 8)\n\n'
             'test result: FAILED. 0 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n')

    def test_noisy_port_reuse_doctest_is_classified_port_reuse(self):
        failures, omitted, _ = s.parse_doctest_failures(self.NOISY)
        by_name = {item['name']: item for item in failures}
        self.assertEqual(omitted, 0)
        self.assertEqual(s.classify('doctest', 'src/lib.rs - (line 1)', by_name['src/lib.rs - (line 1)']['excerpt'])[0],
                         'port reuse')
        self.assertEqual(s.classify('doctest', 'src/lib.rs - (line 8)', by_name['src/lib.rs - (line 8)']['excerpt'])[0],
                         'unknown')
        self.assertLessEqual(len(by_name['src/lib.rs - (line 1)']['excerpt']), 4000)
        summary = s.render('windows-latest', [(Path('support.json'), {'conclusion': 'failure', 'doctests': 'failure',
                                                                     'doctestFailures': failures})])
        self.assertIn('| support | doctest manta_a | src/lib.rs - (line 1) | port reuse |', summary)

    def test_same_doctest_name_in_two_crates_keeps_each_block(self):
        report = ('   Doc-tests a\ntest src/lib.rs - f (line 3) ... FAILED\n\nfailures:\n\n'
                  '---- src/lib.rs - f (line 3) stdout ----\nleft: 1 right: 2\n\nfailures:\n    src/lib.rs - f (line 3)\n\n'
                  'test result: FAILED. 0 passed; 1 failed\n'
                  '   Doc-tests b\ntest src/lib.rs - f (line 3) ... FAILED\n\nfailures:\n\n'
                  '---- src/lib.rs - f (line 3) stdout ----\nos error 10048\n\nfailures:\n    src/lib.rs - f (line 3)\n\n'
                  'test result: FAILED. 0 passed; 1 failed\n')
        failures, _, _ = s.parse_doctest_failures(report)
        self.assertEqual([(f['crate'], s.classify('doctest', f['name'], f['excerpt'])[0]) for f in failures],
                         [('a', 'unknown'), ('b', 'port reuse')])

    def test_doctest_failures_past_the_cap_are_counted_and_reported(self):
        report = '   Doc-tests a\n' + ''.join(f'test src/lib.rs - f{i} (line {i}) ... FAILED\n' for i in range(70))
        failures, omitted, _ = s.parse_doctest_failures(report)
        self.assertEqual((len(failures), omitted), (50, 20))
        summary = s.render('windows-latest', [(Path('support.json'), {
            'conclusion': 'failure', 'doctests': 'failure', 'doctestFailures': failures,
            'omittedDoctestFailures': omitted})])
        self.assertIn('20 additional failed doctests omitted from evidence', summary)

    def test_doctest_step_failing_without_a_name_reports_the_log_tail(self):
        log = 'error[E0425]: cannot find value `x` in this scope\nerror: could not compile `a`\n'
        failures, omitted, tail = s.parse_doctest_failures(log)
        self.assertEqual((failures, omitted), ([], 0))
        summary = s.render('windows-latest', [(Path('support.json'), {
            'conclusion': 'failure', 'doctests': 'failure', 'doctestFailures': failures,
            'doctestOutputTail': tail})])
        self.assertIn('could not compile', summary)

    def test_crlf_doctest_log_parses(self):
        failures, _, _ = s.parse_doctest_failures(self.NOISY.replace('\n', '\r\n'))
        self.assertEqual(len(failures), 2)
        self.assertTrue(all(f['excerpt'] for f in failures))

    def test_excerpts_past_the_cap_are_counted(self):
        results = [{'key': f'pkg::test::t{i}', 'exitCode': 1, 'failedTests': ['x'],
                    'failureExcerpts': {'x': 'boom'}} for i in range(55)]
        out = s.render('windows-latest', [(Path('shard.json'), {'index': 0, 'results': results})])
        self.assertIn('5 more failing tests omitted from table', out)
        self.assertIn('5 more failure excerpts omitted', out)

    def test_malformed_nested_shard_keeps_valid_failure(self):
        valid = {'index': 1, 'results': [{'key': 'pkg::test::health_command', 'exitCode': 1,
                 'failedTests': ['health_escapes_peer_text'], 'failureExcerpts': {}}]}
        out = s.render('windows-latest', [(Path('bad.json'), {'index': 0, 'results': None}),
                                           (Path('mixed.json'), {'index': 2, 'results': [None]}),
                                           (Path('valid.json'), valid)])
        self.assertIn('malformed results list', out)
        self.assertIn('malformed result entry', out)
        self.assertIn('health_escapes_peer_text', out)

    def test_failing_test_row_names_os_shard_binary_test_and_class(self):
        record = {'index': 2, 'results': [{'key': 'pkg::test::health_command', 'exitCode': 1,
                   'failedTests': ['health_escapes_peer_text'], 'failureExcerpts': {'health_escapes_peer_text': 'panic'}}]}
        for runner, heading in (('windows-latest', 'Windows (windows-latest)'),
                                ('ubuntu-latest', 'Linux (ubuntu-latest)'),
                                ('macos-latest', 'macOS (macos-latest)'),
                                ('other-runner', 'other-runner')):
            out = s.render(runner, [(Path('shard.json'), record)])
            for expected in (heading, '| 2 | health_command | health_escapes_peer_text | port reuse |', '<details>'):
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

    def test_repeat_failure_carries_class_and_excerpt(self):
        repeat = {'ok': False, 'key': 'pkg::test::health_command', 'test': 'health_escapes_peer_text',
                  'runs': [{'iteration': 3, 'ok': False, 'excerpt': 'bind: os error 10048'}]}
        out = s.render('windows-latest', [(Path('shard'), {'index': 2, 'results': [], 'repeatResults': [repeat]})])
        self.assertIn('socket-test repeat failed: health_escapes_peer_text in health_command', out)
        self.assertIn('class: port reuse', out)
        self.assertIn('os error 10048', out)

    def test_non_object_evidence_does_not_suppress_valid_failures(self):
        record = {'index': 2, 'results': [{'key': 'pkg::test::health_command', 'exitCode': 1,
                   'failedTests': ['health_escapes_peer_text'], 'failureExcerpts': {'health_escapes_peer_text': 'panic'}}]}
        out = s.render('windows-latest', [(Path('array.json'), [1, 2]), (Path('shard.json'), record)])
        self.assertIn('| 2 | health_command | health_escapes_peer_text | port reuse |', out)
        self.assertIn('array.json', out)
        self.assertIn('not a JSON object', out)

    def test_non_object_evidence_file_exits_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            array = Path(directory)/'array.json'
            array.write_text('[1, 2]', encoding='utf-8')
            output = Path(directory)/'summary'
            self.assertEqual(s.main(['--os', 'windows-latest', '--append', str(output), str(array)]), 0)
            self.assertIn('not a JSON object', output.read_text(encoding='utf-8'))

    def test_all_green_is_one_line(self):
        out = s.render('windows-latest', [(Path('shard'), {'index': 0, 'results': [{'exitCode': 0}]})])
        self.assertEqual(len(out.splitlines()), 1)
        self.assertIn('no failures', out)

    def test_no_evidence_exits_zero(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)/'summary'
            self.assertEqual(s.main(['--os', 'windows-latest', '--append', str(output)]), 0)
            self.assertIn('no failures', output.read_text())
