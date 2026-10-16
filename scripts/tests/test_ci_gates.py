"""MAN-156: CI gates for dependencies (cargo deny), rustdoc, Windows releases and PR titles;
MAN-240: the Dependabot diff verifier's tests in the required `test` job.

stdlib unittest only -- workflows are read as text with test_ci_trust_boundary.py's helpers.
Run: python3 -m unittest scripts.tests.test_ci_gates -v
"""
import contextlib
import importlib.util
import io
import os
import re
import subprocess
import tempfile
import tomllib
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


wf = _load("_ci_workflow_text", os.path.join(HERE, "test_ci_trust_boundary.py"))
release_ci = _load("check_release_ci", os.path.join(ROOT, "scripts", "check-release-ci.py"))
title = _load("check_pr_title", os.path.join(ROOT, "scripts", "check-pr-title.py"))


def steps_running(job_lines, command):
    return [s for s in wf.steps(job_lines) if (wf.run_body(s) or "").strip() == command]


class RustdocGateTests(unittest.TestCase):
    def setUp(self):
        self.test_job = wf.job_block(wf.read("ci-full.yml"), "test")

    def test_test_job_builds_docs_with_warnings_denied(self):
        found = steps_running(self.test_job, "cargo doc --no-deps --workspace --locked")
        self.assertEqual(len(found), 1)
        self.assertEqual(wf.field(found[0], "if"), "runner.os == 'Linux'")
        self.assertEqual(wf.field(wf.sub(found[0], "env"), "RUSTDOCFLAGS"), "-D warnings")

    def test_docs_build_before_the_test_suite(self):
        all_steps = wf.steps(self.test_job)
        doc = next(i for i, s in enumerate(all_steps)
                   if (wf.run_body(s) or "").strip() == "cargo doc --no-deps --workspace --locked")
        tests = next(i for i, s in enumerate(all_steps)
                     if (wf.run_body(s) or "").strip() == "cargo test --workspace --locked")
        self.assertLess(doc, tests)

    def test_this_module_runs_in_the_required_test_job(self):
        found = steps_running(self.test_job, "python3 -m unittest scripts.tests.test_ci_gates -v")
        self.assertEqual(len(found), 1)
        self.assertEqual(wf.field(found[0], "if"), "runner.os == 'Linux'")


VERIFIER_TESTS = "python3 -m unittest scripts.tests.test_verify_dependabot_diff_safe -v"


def setup_python_steps(job_lines):
    return [s for s in wf.steps(job_lines)
            if (wf.field(s, "uses") or "").startswith("actions/setup-python@")]


class VerifierTestsGateTests(unittest.TestCase):
    """MAN-240: the Dependabot diff verifier's tests ride the required `test` job, under the
    Python dependabot-auto-merge.yml runs the verifier with."""

    def setUp(self):
        self.test_job = wf.job_block(wf.read("ci-full.yml"), "test")
        self.verify_job = wf.job_block(wf.read("dependabot-auto-merge.yml"), "verify")

    def test_verifier_tests_run_in_the_required_test_job_on_linux(self):
        found = steps_running(self.test_job, VERIFIER_TESTS)
        self.assertEqual(len(found), 1)
        self.assertEqual(wf.field(found[0], "if"), "runner.os == 'Linux'")

    def test_verifier_tests_use_the_auto_merge_python(self):
        ours, theirs = setup_python_steps(self.test_job), setup_python_steps(self.verify_job)
        self.assertEqual(len(ours), 1)
        self.assertEqual(len(theirs), 1)
        self.assertEqual(wf.field(ours[0], "uses").split()[0], wf.field(theirs[0], "uses").split()[0])
        version = wf.field(wf.sub(theirs[0], "with"), "python-version")
        self.assertIsNotNone(version)
        self.assertEqual(wf.field(wf.sub(ours[0], "with"), "python-version"), version)
        self.assertEqual(wf.field(ours[0], "if"), "runner.os == 'Linux'")
        all_steps = wf.steps(self.test_job)
        setup = all_steps.index(ours[0])
        tests = next(i for i, s in enumerate(all_steps)
                     if (wf.run_body(s) or "").strip() == VERIFIER_TESTS)
        self.assertLess(setup, tests)


COPPA_GIT = "https://github.com/HagaleTechnologies/coppa.git"
PERMISSIVE = {"0BSD", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
              "BSL-1.0", "ISC", "MIT", "Unicode-3.0", "Unlicense", "Zlib"}
SHA_PIN = re.compile(r"^taiki-e/install-action@[0-9a-f]{40}$")


class SupplyChainWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = wf.read("supply-chain.yml")
        self.audit = wf.job_block(self.text, "audit")

    def test_required_job_keeps_its_name(self):
        self.assertEqual(wf.field(self.audit, "name"), "Supply Chain")

    def test_runs_cargo_deny_installed_from_a_sha_pinned_action(self):
        installs = [s for s in wf.steps(self.audit)
                    if (wf.field(s, "uses") or "").startswith("taiki-e/install-action@")]
        self.assertEqual(len(installs), 1)
        self.assertRegex(wf.field(installs[0], "uses").split()[0], SHA_PIN)
        self.assertEqual(wf.field(wf.sub(installs[0], "with"), "tool"), "cargo-deny@0.20.2")
        self.assertEqual(len(steps_running(self.audit, "cargo deny --locked check")), 1)

    def test_cargo_audit_is_gone(self):
        code = "\n".join(wf.code_lines(self.text))
        self.assertNotIn("rustsec/audit-check", code)
        self.assertNotIn("audit.toml", code)

    def test_deny_toml_change_triggers_the_scan(self):
        self.assertIn("- 'deny.toml'", "\n".join(wf.code_lines(wf.job_block(self.text, "changes"))))

    def test_daily_schedule_always_scans(self):
        self.assertRegex(self.text, r"(?m)^  schedule:\n    - cron: '[^']+'$")
        outputs = wf.sub(wf.job_block(self.text, "changes"), "outputs")
        self.assertIn("github.event_name == 'schedule' && 'true'", wf.field(outputs, "deps"))

    def test_fail_closed_step_kept(self):
        self.assertTrue(any(wf.field(s, "if") == "needs.changes.result != 'success'"
                            for s in wf.steps(self.audit)))


class DenyPolicyTests(unittest.TestCase):
    def setUp(self):
        with open(os.path.join(ROOT, "deny.toml"), "rb") as f:
            self.cfg = tomllib.load(f)

    def test_every_feature_is_in_the_graph(self):
        self.assertIs(self.cfg["graph"]["all-features"], True)

    def test_advisory_ignores_carry_a_reason(self):
        adv = self.cfg["advisories"]
        self.assertEqual(adv.get("yanked"), "deny")
        for entry in adv.get("ignore", []):
            self.assertRegex(entry["id"], r"^RUSTSEC-\d{4}-\d{4}$")
            self.assertTrue(entry.get("reason", "").strip(), entry)

    def test_only_permissive_licenses(self):
        allow = set(self.cfg["licenses"]["allow"])
        self.assertLessEqual(allow, PERMISSIVE)
        for lic in allow:
            self.assertNotRegex(lic, r"GPL|MPL|EUPL|CDDL|SSPL|CC-BY-SA")

    def test_sources_are_crates_io_and_coppa_only(self):
        src = self.cfg["sources"]
        self.assertEqual(src["unknown-registry"], "deny")
        self.assertEqual(src["unknown-git"], "deny")
        self.assertEqual(src["allow-git"], [COPPA_GIT])

    def test_wildcards_denied_except_workspace_paths(self):
        self.assertEqual(self.cfg["bans"]["wildcards"], "deny")
        self.assertIs(self.cfg["bans"]["allow-wildcard-paths"], True)

    def test_deny_toml_has_a_code_owner(self):
        with open(os.path.join(ROOT, ".github", "CODEOWNERS"), encoding="utf-8") as f:
            self.assertRegex(f.read(), r"(?m)^/deny\.toml\s+@thagale$")


class CodeOwnersTests(unittest.TestCase):
    """MAN-247: the pipeline's verify list, build scripts and checkout attributes need owner
    review; build.rs and .gitattributes have no leading slash, so nested ones are covered."""

    def setUp(self):
        with open(os.path.join(ROOT, ".github", "CODEOWNERS"), encoding="utf-8") as f:
            self.text = f.read()

    def test_pipeline_definition_has_a_code_owner(self):
        self.assertRegex(self.text, r"(?m)^/\.catalyst/\s+@thagale$")

    def test_build_scripts_at_any_depth_have_a_code_owner(self):
        self.assertRegex(self.text, r"(?m)^build\.rs\s+@thagale$")

    def test_attributes_files_at_any_depth_have_a_code_owner(self):
        self.assertRegex(self.text, r"(?m)^\.gitattributes\s+@thagale$")


SHA = "cb4fff9290f643bc4d9ebc1ef26fd8707ed9852d"
CHECKS = ("test (ubuntu-latest)", "test (macos-latest)", "test (windows-latest)")
LEGS = CHECKS + ("test-hpsdr (ubuntu-latest)", "test-hpsdr (macos-latest)", "test-hpsdr (windows-latest)")


def run(number=1, status="completed", conclusion="success"):
    return {"id": 100 + number, "run_number": number, "status": status, "conclusion": conclusion,
            "html_url": f"https://example.invalid/runs/{100 + number}"}


def jobs(**overrides):
    out = [{"name": n, "conclusion": overrides.get(n, "success")} for n in LEGS]
    out += [{"name": "test-soapy (ubuntu-latest)", "conclusion": "success"},
            {"name": "Create required test check-runs on the reviewed SHA", "conclusion": "skipped"}]
    return [j for j in out if j["conclusion"] != "absent"]


class WindowsVisibilityTests(unittest.TestCase):
    def setUp(self):
        self.text = wf.read("ci-full.yml")

    def test_windows_stays_in_the_test_matrix(self):
        self.assertIn("os: [ubuntu-latest, macos-latest, windows-latest]",
                      "\n".join(wf.job_block(self.text, "test")))

    def test_create_ci_check_mints_the_windows_check_run(self):
        job = wf.job_block(self.text, "create-ci-check")
        body = wf.run_body(wf.step_by_id(job, "create"))
        self.assertIn('-f name="test (windows-latest)"', body)
        self.assertIn('windows_check_id=$windows_id', body)
        self.assertEqual(wf.field(wf.sub(job, "outputs"), "windows_check_id"),
                         "${{ steps.create.outputs.windows_check_id }}")

    def test_create_ci_check_fails_closed_for_every_leg(self):
        # MAN-371: all three check-runs are required, so a failed POST must fail the step; a
        # warning and an empty id would leave the PR on "Expected" behind a green job.
        body = wf.run_body(wf.step_by_id(wf.job_block(self.text, "create-ci-check"), "create"))
        for failing in ("", *CHECKS):
            with self.subTest(failing=failing or "none"), tempfile.TemporaryDirectory() as tmp:
                gh = os.path.join(tmp, "gh")
                with open(gh, "w", encoding="utf-8") as f:
                    f.write('#!/bin/sh\ncase "$*" in *"name=$FAIL_NAME"*) exit 1;; esac\n'
                            'case "$*" in *ubuntu*) echo 11;; *macos*) echo 12;; *windows*) echo 13;; esac\n')
                os.chmod(gh, 0o755)
                script, out = os.path.join(tmp, "create.sh"), os.path.join(tmp, "out")
                with open(script, "w", encoding="utf-8") as f:
                    f.write(body)
                open(out, "w").close()
                env = {"PATH": f"{tmp}:{os.environ.get('PATH', '/usr/bin:/bin')}", "GITHUB_OUTPUT": out,
                       "REPO": "o/r", "HEAD_SHA": "0" * 40, "FAIL_NAME": failing or "-"}
                # The step sets no `shell:`, so the runner runs it as `bash -e {0}`.
                proc = subprocess.run(["bash", "-e", script], env=env, capture_output=True, text=True)
                with open(out, encoding="utf-8") as f:
                    written = f.read().split()
                if not failing:
                    self.assertEqual(proc.returncode, 0, proc.stderr)
                    self.assertEqual(written, ["ubuntu_check_id=11", "macos_check_id=12",
                                               "windows_check_id=13"])
                else:
                    self.assertNotEqual(proc.returncode, 0, f"a failed {failing} POST must fail the step")
                    self.assertNotIn("::warning::", proc.stdout)
                    self.assertEqual([w for w in written if w.endswith("=")], [], "no empty check-run id")

    def test_relay_reports_the_windows_leg(self):
        relay = wf.job_block(self.text, "relay-check-runs")
        step = wf.steps(relay)[-1]
        self.assertEqual(wf.field(wf.sub(step, "env"), "WINDOWS_CHECK_ID"),
                         "${{ needs.create-ci-check.outputs.windows_check_id }}")
        self.assertIn('relay "test (windows-latest)" "$WINDOWS_CHECK_ID"', wf.run_body(step))


class ReleaseCiVerdictTests(unittest.TestCase):
    def test_green_run_passes(self):
        ok, msg = release_ci.verdict(SHA, [run()], jobs())
        self.assertTrue(ok, msg)
        self.assertTrue(msg.startswith("OK: "))

    def test_windows_only_failure_is_refused_and_named(self):
        ok, msg = release_ci.verdict(SHA, [run(conclusion="failure")],
                                     jobs(**{"test (windows-latest)": "failure"}))
        self.assertFalse(ok)
        self.assertIn("test (windows-latest)=failure", msg)

    def test_hpsdr_windows_failure_is_refused(self):  # the cb4fff9 shape
        ok, msg = release_ci.verdict(SHA, [run(conclusion="failure")],
                                     jobs(**{"test-hpsdr (windows-latest)": "failure"}))
        self.assertFalse(ok)
        self.assertIn("test-hpsdr (windows-latest)=failure", msg)

    def test_unfinished_run_is_refused(self):
        ok, msg = release_ci.verdict(SHA, [run(status="in_progress", conclusion=None)], [])
        self.assertFalse(ok)
        self.assertIn("in_progress", msg)

    def test_missing_run_is_refused(self):
        ok, msg = release_ci.verdict(SHA, [], [])
        self.assertFalse(ok)
        self.assertIn("no ci-full.yml push run", msg)

    def test_a_leg_missing_from_the_matrix_is_refused(self):
        ok, msg = release_ci.verdict(SHA, [run()], jobs(**{"test (windows-latest)": "absent"}))
        self.assertFalse(ok)
        self.assertIn("test (windows-latest)=missing", msg)

    def test_newest_run_decides(self):
        ok, _ = release_ci.verdict(SHA, [run(1, conclusion="failure"), run(2)], jobs())
        self.assertTrue(ok)

    def test_bad_sha_is_a_usage_error_without_an_api_call(self):
        self.assertEqual(release_ci.main(["x", "HagaleTechnologies/manta", "main"]), 2)


class ReleaseCiWiringTests(unittest.TestCase):
    CMD = 'python3 scripts/check-release-ci.py "$GITHUB_REPOSITORY" "$RELEASE_COMMIT"'

    def setUp(self):
        self.job = wf.job_block(wf.read("release-publish.yml"), "validate-tag")
        self.steps = wf.steps(self.job)

    def test_validate_tag_runs_the_gate_on_tag_pushes(self):
        found = steps_running(self.job, self.CMD)
        self.assertEqual(len(found), 1)
        self.assertEqual(wf.field(found[0], "if"), "github.event_name == 'push'")
        env = wf.sub(found[0], "env")
        self.assertEqual(wf.field(env, "GH_TOKEN"), "${{ github.token }}")
        self.assertEqual(wf.field(env, "RELEASE_COMMIT"), "${{ github.sha }}")

    def test_gate_sits_between_the_ancestry_check_and_the_build(self):
        bodies = [(wf.run_body(s) or "") for s in self.steps]
        ancestry = next(i for i, b in enumerate(bodies) if "check-release-ancestry.sh" in b)
        gate = next(i for i, b in enumerate(bodies) if "check-release-ci.py" in b)
        build = next(i for i, b in enumerate(bodies) if "cargo build --release -p manta-cli" in b)
        self.assertLess(ancestry, gate)
        self.assertLess(gate, build)

    def test_validate_tag_may_read_actions_and_nothing_more(self):
        perms = [l.strip() for l in wf.code_lines(wf.sub(self.job, "permissions")[1:]) if l.strip()]
        self.assertEqual(sorted(perms), ["actions: read", "contents: read"])


ACCEPT = [
    "feat: MAN-133 — manta should support shell completions and a man page like other CLI tools operators already…",
    "fix(dsp,engine,cli,server,testkit): MAN-312 — keep a static crash out of every track",
    "chore(deps): bump syn from 2.0.119 to 3.0.4",
    "ci: MAN-156 — CI should catch dependency, documentation, and platform regressions",
    "feat(cli)!: drop --old-flag",
    'Revert "feat: MAN-301 — grade every spot"',
]
REJECT = [
    "Add Catalyst starter skills",          # PR #89
    "measure(manta): MAN-19 24h unattended soak harness",  # PR #70: unknown type
    "M1: live audio, one signal",           # PR #5
    "Feat: capitalised type",
    "feat:no space",
    "feat: ",
    "fix(): empty scope",
    "wip: not a type",
]


class PrTitleRuleTests(unittest.TestCase):
    def test_conventional_titles_pass(self):
        for t in ACCEPT:
            with self.subTest(title=t):
                self.assertIsNone(title.problem(t))

    def test_other_titles_fail_with_a_reason(self):
        for t in REJECT:
            with self.subTest(title=t):
                self.assertTrue(title.problem(t))

    def test_unknown_type_lists_the_allowed_ones(self):
        self.assertIn("build, chore, ci, docs, feat, fix, perf, refactor, revert, style, test",
                      title.problem("wip: x"))

    def test_main_exit_codes_and_annotation(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(title.main(["x", "Add Catalyst starter skills"]), 1)
        self.assertTrue(out.getvalue().startswith("::error title=PR title is not a Conventional Commits header::"))
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(title.main(["x", ACCEPT[0]]), 0)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(title.main(["x"]), 2)


class PrTitleWorkflowTests(unittest.TestCase):
    def setUp(self):
        self.text = wf.read("pr-title.yml")
        self.job = wf.job_block(self.text, "title")

    def test_triggers(self):
        self.assertRegex(self.text, r"(?m)^  pull_request:\n    types: \[opened, edited, reopened, synchronize\]$")
        self.assertRegex(self.text, r"(?m)^  merge_group:\n    types: \[checks_requested\]$")

    def test_job_is_the_check_named_pr_title_and_always_runs(self):
        self.assertEqual(wf.field(self.job, "name"), "PR title")
        self.assertIsNone(wf.field(self.job, "if"))

    def test_title_reaches_the_script_through_env_only(self):
        found = steps_running(self.job, 'python3 scripts/check-pr-title.py "$PR_TITLE"')
        self.assertEqual(len(found), 1)
        self.assertEqual(wf.field(wf.sub(found[0], "env"), "PR_TITLE"),
                         "${{ github.event.pull_request.title }}")
        for step in wf.steps(self.job):
            self.assertNotIn("${{", wf.run_body(step) or "")

    def test_merge_group_passes_and_no_secret_is_used(self):
        self.assertTrue(any(wf.field(s, "if") == "github.event_name == 'merge_group'"
                            for s in wf.steps(self.job)))
        self.assertNotIn("secrets.", "\n".join(wf.code_lines(self.text)))


if __name__ == "__main__":
    unittest.main()
