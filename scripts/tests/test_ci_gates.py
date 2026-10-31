"""MAN-156: CI gates for dependencies (cargo deny), rustdoc, Windows releases and PR titles;
MAN-240: the Dependabot diff verifier's tests in the required `test` job.
MAN-245: the Codex gate's verdict (a complete thread read, a check-run title naming what produced
it) and the record of which source may report each required check.

stdlib unittest only -- workflows are read as text with test_ci_trust_boundary.py's helpers.
Run: python3 -m unittest scripts.tests.test_ci_gates -v
"""
import contextlib
import glob
import importlib.util
import io
import json
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
sources = _load("required_check_sources", os.path.join(ROOT, "scripts", "required-check-sources.py"))


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
# MAN-318: CDLA-Permissive-2.0 is webpki-roots' (Mozilla's CA list, as data).
PERMISSIVE = {"0BSD", "Apache-2.0", "Apache-2.0 WITH LLVM-exception", "BSD-2-Clause", "BSD-3-Clause",
              "BSL-1.0", "CDLA-Permissive-2.0", "ISC", "MIT", "Unicode-3.0", "Unlicense", "Zlib"}
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


def run(number=1, status="completed", conclusion="success"):
    return {"id": 100 + number, "run_number": number, "status": status, "conclusion": conclusion,
            "html_url": f"https://example.invalid/runs/{100 + number}"}


def jobs(**overrides):
    out = [{"name": n, "conclusion": overrides.get(n, "success")}
           for n in CHECKS + ("test-soapy (ubuntu-latest)",)]
    out += [{"name": "Create required test check-runs on the reviewed SHA", "conclusion": "skipped"}]
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
    def test_green_default_jobs_pass_without_hpsdr_jobs(self):
        ok, msg = release_ci.verdict(SHA, [run()], jobs())
        self.assertTrue(ok, msg)
        self.assertTrue(msg.startswith("OK: "))

    def test_windows_only_failure_is_refused_and_named(self):
        ok, msg = release_ci.verdict(SHA, [run(conclusion="failure")],
                                     jobs(**{"test (windows-latest)": "failure"}))
        self.assertFalse(ok)
        self.assertIn("test (windows-latest)=failure", msg)

    def test_surviving_soapy_job_failure_is_refused(self):
        ok, msg = release_ci.verdict(SHA, [run(conclusion="failure")],
                                     jobs(**{"test-soapy (ubuntu-latest)": "failure"}))
        self.assertFalse(ok)
        self.assertIn("test-soapy (ubuntu-latest)=failure", msg)

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


# Every call is recorded as its arguments, each NUL-terminated, then a record separator, so a test
# can match one argument exactly (a title that is a prefix of another must not match it).
GH_STUB_HEADER = "#!/bin/bash\n{ printf '%s\\0' \"$@\"; printf '\\036'; } >> \"$GH_CALLS\"\n"


def run_step(body, env, gh_script, *, tmp):
    """Run a workflow run: body under GitHub's bash flags with a stub gh first on PATH.
    gh_script is the stub's bash text, run after it records the call in $GH_CALLS.
    Returns (CompletedProcess, GITHUB_OUTPUT text, gh calls as argument lists)."""
    bindir = os.path.join(tmp, "bin")
    os.makedirs(bindir, exist_ok=True)
    gh = os.path.join(bindir, "gh")
    with open(gh, "w", encoding="utf-8") as f:
        f.write(GH_STUB_HEADER + gh_script)
    os.chmod(gh, 0o755)
    script, out, calls = (os.path.join(tmp, n) for n in ("run.sh", "out", "calls"))
    with open(script, "w", encoding="utf-8") as f:
        f.write(body)
    for path in (out, calls):
        open(path, "w").close()
    full_env = {"PATH": f"{bindir}:/usr/bin:/bin", "GITHUB_OUTPUT": out, "GH_CALLS": calls, **env}
    proc = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", script],
                          cwd=tmp, env=full_env, capture_output=True, text=True)
    with open(out, encoding="utf-8") as f:
        written = f.read()
    with open(calls, encoding="utf-8") as f:
        recorded = [rec.split("\0")[:-1] for rec in f.read().split("\x1e") if rec]
    return proc, written, recorded


def step_by_name(block, name):
    """The step whose 'name:' is name, or None."""
    for step in wf.steps(block):
        if wf.field(step, "name") == name:
            return step
    return None


def gate_step(job, *, sid=None, name=None):
    block = wf.job_block(wf.read("wait-for-codex.yml"), job)
    step = wf.step_by_id(block, sid) if sid else step_by_name(block, name)
    if step is None:
        raise AssertionError(f"wait-for-codex.yml {job}: no step {sid or name!r}")
    return step


def output_lines(written):
    return written.splitlines()


def has_call(calls, *needles):
    """True when one recorded call carries every needle as an exact argument."""
    return any(all(n in call for n in needles) for call in calls)


def arg_prefixed(call, prefix):
    return [a for a in call if a.startswith(prefix)]


INCOMPLETE_DEF = ('def thread_list_incomplete: (.pageInfo.hasNextPage != false) or '
                  '([.nodes[] | select(.isResolved == false) | select((.comments.totalCount | type) != "number" '
                  'or .comments.totalCount > (.comments.nodes | length))] | length > 0);')
CODEX_BOT = "chatgpt-codex-connector[bot]"
GATE_REPO = "HagaleTechnologies/manta"
GATE_PR = "999"
HEAD_SHA = "a" * 40
LABEL = "codex-clean:" + "a" * 12
RUN_STARTED = "2026-10-16T10:00:00Z"
CHECK_NAME = "Codex review clean (gate before expensive downstream jobs)"


def review_threads(n, *, unresolved=(), long=(), has_next=False, page_info=True):
    """The GraphQL answer for a PR's first page of review threads, as GitHub returns it.
    Threads 1..n; those numbered in `unresolved` are open and started by Codex after RUN_STARTED,
    the rest resolved. A thread numbered in `long` returns 50 of its 51 comments."""
    nodes = []
    for i in range(1, n + 1):
        returned = 50 if i in long else 1
        comments = [{"author": {"login": CODEX_BOT}, "createdAt": "2026-10-16T10:05:00Z",
                     "originalCommit": {"oid": "b" * 40}} for _ in range(returned)]
        nodes.append({"id": f"PRRT_{i}", "isResolved": i not in unresolved, "path": "src/lib.rs",
                      "comments": {"totalCount": 51 if i in long else returned, "nodes": comments}})
    threads = {"nodes": nodes}
    if page_info:
        threads["pageInfo"] = {"hasNextPage": has_next}
    return {"data": {"repository": {"pullRequest": {"reviewThreads": threads}}}}


VERDICT_GH = """case "$*" in
  *"/pulls/"*"--jq .head.sha"*) echo "$HEAD_SHA" ;;
  "api graphql"*) cat "$FIXTURE" ;;
  "label create"*) ;;
  *"/labels"*) ;;
  *) echo "unexpected gh call: $*" >&2; exit 1 ;;
esac
"""
UPDATE_GH = """case "$*" in
  *"check-runs"*"--jq"*) echo 12345 ;;
esac
"""


class CodexThreadListCompletenessTests(unittest.TestCase):
    """MAN-245 scenario 1: a review-thread list the gate could not read in full never reports clean."""

    def setUp(self):
        self.baseline = wf.run_body(gate_step("codex-review-response", sid="baseline"))
        self.verdict_step = gate_step("codex-review-response", sid="compute-verdict")
        self.verdict = wf.run_body(self.verdict_step)
        self.update = wf.run_body(gate_step("codex-review-response", name="Update the check-run"))

    def test_both_thread_queries_request_page_info_and_comment_totals(self):
        for label, body in (("baseline", self.baseline), ("compute-verdict", self.verdict)):
            with self.subTest(step=label):
                self.assertIn("pageInfo { hasNextPage }", body)
                self.assertIn("comments(first: 50) { totalCount nodes", body)

    def test_completeness_def_is_identical_in_both_steps(self):
        for label, body in (("baseline", self.baseline), ("compute-verdict", self.verdict)):
            with self.subTest(step=label):
                self.assertIn(INCOMPLETE_DEF, body)

    def test_compute_verdict_body_has_no_expressions(self):
        self.assertNotIn("${{", self.verdict)
        env = wf.sub(self.verdict_step, "env")
        self.assertEqual(wf.field(env, "BASELINE"), "${{ steps.baseline.outputs.baseline_pure }}")
        self.assertEqual(wf.field(env, "BASELINE_INITIATED"), "${{ steps.baseline.outputs.baseline_initiated }}")
        self.assertEqual(wf.field(env, "BASELINE_INCOMPLETE"), "${{ steps.baseline.outputs.incomplete }}")

    def _verdict(self, fixture, baseline_incomplete=""):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "threads.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(fixture, f)
            env = {"GH_TOKEN": "x", "RESOLVE_TOKEN": "y", "REPO": GATE_REPO, "PR": GATE_PR,
                   "HEAD_SHA": HEAD_SHA, "RUN_STARTED": RUN_STARTED, "IS_GENUINE_SYNC": "true",
                   "BASELINE": "", "BASELINE_INITIATED": "", "BASELINE_INCOMPLETE": baseline_incomplete,
                   "FIXTURE": path}
            return run_step(self.verdict, env, VERDICT_GH, tmp=tmp)

    def test_verdict_table(self):
        cases = [  # (case, fixture, BASELINE_INCOMPLETE, exit, incomplete, label POST, log)
            ("over-limit", review_threads(100, has_next=True), "", 1, True, False,
             "Review thread list incomplete"),
            ("truncated thread", review_threads(1, unresolved={1}, long={1}), "", 1, True, False,
             "Review thread list incomplete"),
            ("baseline incomplete", review_threads(3), "true", 1, True, False,
             "Review thread list incomplete"),
            ("missing pageInfo", review_threads(3, page_info=False), "", 1, True, False,
             "Review thread list incomplete"),
            ("control: concern in page", review_threads(100, unresolved={37}), "", 1, False, False,
             "Codex raised 1 new concern(s)"),
            ("control: exactly 100 clean", review_threads(100), "", 0, False, True,
             f"Applied {LABEL}."),
            ("resolved long thread", review_threads(1, long={1}), "", 0, False, True,
             "Applied codex-clean:"),
        ]
        for case, fixture, base_inc, code, incomplete, labelled, log in cases:
            with self.subTest(case=case):
                proc, written, calls = self._verdict(fixture, base_inc)
                self.assertEqual(proc.returncode, code, proc.stdout + proc.stderr)
                self.assertEqual("incomplete=true" in output_lines(written), incomplete, written)
                label_posts = [c for c in calls if f"repos/{GATE_REPO}/issues/{GATE_PR}/labels" in c]
                self.assertEqual(bool(label_posts), labelled, calls)
                self.assertFalse(any("resolveReviewThread" in a for c in calls for a in c), calls)
                self.assertIn(log, proc.stdout)
                if incomplete:
                    self.assertIn("::error::Review thread list incomplete", proc.stdout)

    def _baseline(self, fixture):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "threads.json")
            with open(path, "w", encoding="utf-8") as f:
                json.dump(fixture, f)
            env = {"GH_TOKEN": "x", "REPO": GATE_REPO, "PR": GATE_PR, "SINCE": RUN_STARTED, "FIXTURE": path}
            return run_step(self.baseline, env, VERDICT_GH, tmp=tmp)

    def test_baseline_step_records_incompleteness(self):
        for case, fixture, incomplete in (("over-limit", review_threads(100, has_next=True), "true"),
                                          ("exactly 100 clean", review_threads(100), "false")):
            with self.subTest(case=case):
                proc, written, _ = self._baseline(fixture)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertIn(f"incomplete={incomplete}", output_lines(written))
                self.assertIn("baseline_pure<<", written)
                self.assertIn("baseline_initiated<<", written)
                if incomplete == "true":
                    self.assertIn("::warning::", proc.stdout)
                    self.assertIn(f"PR #{GATE_PR}", proc.stdout)
                else:
                    self.assertNotIn("::warning::", proc.stdout)

    def _update(self, **env):
        with tempfile.TemporaryDirectory() as tmp:
            base = {"GH_TOKEN": "x", "DISPATCH_TOKEN": "", "REPO": GATE_REPO, "HEAD_SHA": HEAD_SHA,
                    "OUTCOME": "failure", "DISPATCH": "false", "INCOMPLETE": ""}
            return run_step(self.update, {**base, **env}, UPDATE_GH, tmp=tmp)

    def test_update_step_titles_an_incomplete_read(self):
        for incomplete, wanted in (("true", "Review thread list incomplete"),
                                   ("", "Codex raised new concerns")):
            with self.subTest(incomplete=incomplete or "unset"):
                proc, _, calls = self._update(OUTCOME="failure", INCOMPLETE=incomplete)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                patches = [c for c in calls if "PATCH" in c]
                self.assertEqual(len(patches), 1, calls)
                self.assertIn("conclusion=failure", patches[0])
                self.assertEqual(arg_prefixed(patches[0], "output[title]="), [f"output[title]={wanted}"])


LABEL_GH = """case "$*" in
  *"/labels"*)
    if [[ " $* " == *" --paginate "* ]]; then cat "$LABELS"; else head -n 100 "$LABELS"; fi ;;
  *"/timeline"*)
    if [ -n "$TIMELINE_FAIL" ]; then exit 1; fi
    cat "$TIMELINE" ;;
  *"/check-runs"*)
    if [ -n "$CHECKS_FAIL" ]; then exit 1; fi
    cat "$CHECKS" ;;
  *) echo "unexpected gh call: $*" >&2; exit 1 ;;
esac
"""
CREATE_GH = """case "$*" in
  *"--jq"*) echo 777 ;;
esac
"""


def labeled(actor, at, name=LABEL):
    return {"event": "labeled", "label": {"name": name}, "actor": {"login": actor}, "created_at": at}


def check_run(app_id, title, conclusion="success", name=CHECK_NAME, completed="2026-10-16T10:00:05Z"):
    # completed_at defaults to just after the gate-verdict label below (10:00:00Z): the verdict
    # step applies the label, then the response job PATCHes the check-run (MAN-245 F1 binding).
    return {"name": name, "app": {"id": app_id}, "conclusion": conclusion,
            "completed_at": completed, "output": {"title": title}}


VERDICT_RUN = check_run(4696353, "Codex review clean")


class CodexCheckRunProvenanceTests(unittest.TestCase):
    """MAN-245 (decision D3): a codex-clean label still passes the gate, and the check-run says
    whether the label was this workflow's own verdict or someone else's override."""

    def setUp(self):
        self.label_body = wf.run_body(gate_step("codex-review-request", sid="existing-label"))
        self.create_body = wf.run_body(gate_step("codex-review-request", sid="create-check"))
        self.update_body = wf.run_body(gate_step("codex-review-response", name="Update the check-run"))

    def _lookup(self, labels, timeline=None, timeline_fail=False, checks=(), checks_fail=False):
        with tempfile.TemporaryDirectory() as tmp:
            lpath, tpath, cpath = (os.path.join(tmp, n) for n in ("labels.txt", "timeline.json", "checks.json"))
            with open(lpath, "w", encoding="utf-8") as f:
                f.write("".join(n + "\n" for n in labels))
            with open(tpath, "w", encoding="utf-8") as f:
                json.dump(timeline or [[]], f)
            with open(cpath, "w", encoding="utf-8") as f:  # pages, as --paginate --slurp returns them
                json.dump([{"total_count": len(checks), "check_runs": list(checks)}], f)
            env = {"GH_TOKEN": "x", "REPO": GATE_REPO, "PR": GATE_PR, "HEAD_SHA": HEAD_SHA,
                   "LABELS": lpath, "TIMELINE": tpath, "TIMELINE_FAIL": "1" if timeline_fail else "",
                   "CHECKS": cpath, "CHECKS_FAIL": "1" if checks_fail else ""}
            return run_step(self.label_body, env, LABEL_GH, tmp=tmp)

    def test_label_lookup(self):
        gate = labeled("github-actions[bot]", "2026-10-16T10:00:00Z")
        human = labeled("thagale", "2026-10-16T11:00:00Z")
        other = labeled("thagale", "2026-10-16T12:00:00Z", name="codex-clean:" + "b" * 12)
        # Any GITHUB_TOKEN workflow labels as github-actions[bot], one defined on an unreviewed
        # branch included; only hagale-agent's completed "Codex review clean" shows a verdict ran.
        forged = [check_run(15368, "Codex review clean"), check_run(4696353, "Codex raised new concerns", "failure"),
                  check_run(4696353, "Codex review clean", name="other")]
        cases = [  # (case, labels, timeline pages, timeline fails, check-runs, check-runs fail,
                   #  already_clean, label_actor, earlier_verdict)
            ("none", ["bug"], None, False, (), False, "false", "", "false"),
            ("gate verdict", [LABEL], [[gate]], False, [VERDICT_RUN], False, "true", "github-actions[bot]", "true"),
            # F1: the verdict check-run must be at or after the latest label event -- the verdict
            # step labels before the response job PATCHes -- so a bot label newer than every
            # verdict check-run was reapplied by another GITHUB_TOKEN workflow: an override.
            ("later bot label, older verdict", [LABEL],
             [[labeled("github-actions[bot]", "2026-10-16T11:00:00Z")]], False,
             [check_run(4696353, "Codex review clean", completed="2026-10-16T09:05:00Z")], False,
             "true", "github-actions[bot]", "false"),
            ("label and verdict check same second", [LABEL], [[gate]], False,
             [check_run(4696353, "Codex review clean", completed="2026-10-16T10:00:00Z")], False,
             "true", "github-actions[bot]", "true"),
            ("workflow label, no verdict check-run", [LABEL], [[gate]], False, forged, False,
             "true", "github-actions[bot]", "false"),
            ("workflow label, check-runs unreadable", [LABEL], [[gate]], False, [VERDICT_RUN], True,
             "true", "github-actions[bot]", "false"),
            ("human override", [LABEL], [[gate], [human, other]], False, [VERDICT_RUN], False,
             "true", "thagale", "false"),
            ("label on page 2", [f"other-{i}" for i in range(100)] + [LABEL], [[human]], False, (), False,
             "true", "thagale", "false"),
            ("timeline unreadable", [LABEL], None, True, [VERDICT_RUN], False, "true", "", "false"),
        ]
        for case, labels, timeline, fail, checks, checks_fail, clean, actor, earlier in cases:
            with self.subTest(case=case):
                proc, written, _ = self._lookup(labels, timeline, fail, checks, checks_fail)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                lines = output_lines(written)
                self.assertIn(f"already_clean={clean}", lines)
                self.assertIn(f"label_actor={actor}", lines)
                self.assertIn(f"earlier_verdict={earlier}", lines)

    def test_later_bot_label_is_titled_an_override(self):
        # F1 (validate attempt 3, PR #327): a github-actions[bot] label applied AFTER an older
        # clean verdict check-run must not read as this workflow's own verdict label. The
        # lookup feeds create-check exactly as the request job does, so the pass-through the
        # validator reproduced -- "Codex review clean (earlier verdict...)" with no warning --
        # becomes override attribution.
        later_label = [[labeled("github-actions[bot]", "2026-10-16T11:00:00Z")]]
        older_verdict = [check_run(4696353, "Codex review clean", completed="2026-10-16T09:05:00Z")]
        genuine_label = [[labeled("github-actions[bot]", "2026-10-16T10:00:00Z")]]
        genuine_verdict = [check_run(4696353, "Codex review clean", completed="2026-10-16T10:00:05Z")]
        for case, timeline, checks, wanted, warning in (
            ("later bot label, older verdict", later_label, older_verdict,
             "Codex review overridden by github-actions[bot] (codex-clean label)", True),
            ("genuine order: label then verdict PATCH", genuine_label, genuine_verdict,
             "Codex review clean (earlier verdict for this revision)", False),
        ):
            with self.subTest(case=case):
                proc, written, _ = self._lookup([LABEL], timeline, checks=checks)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                outs = dict(l.split("=", 1) for l in output_lines(written))
                self.assertEqual(outs["already_clean"], "true")
                proc, written, calls = self._create(ALREADY_CLEAN=outs["already_clean"],
                                                    LABEL_ACTOR=outs["label_actor"],
                                                    EARLIER_VERDICT=outs["earlier_verdict"])
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                posts = [c for c in calls if f"repos/{GATE_REPO}/check-runs" in c]
                self.assertEqual(len(posts), 1, calls)
                self.assertEqual(arg_prefixed(posts[0], "output[title]="), [f"output[title]={wanted}"])
                self.assertEqual("::warning::" in proc.stdout, warning, proc.stdout)

    def _create(self, **env):
        with tempfile.TemporaryDirectory() as tmp:
            base = {"GH_TOKEN": "x", "REPO": GATE_REPO, "HEAD_SHA": HEAD_SHA, "ALREADY_CLEAN": "false",
                    "LABEL_ACTOR": "", "EARLIER_VERDICT": "false", "IS_EXCLUDED_DEPENDABOT": "false",
                    "APP_TOKEN_MINTED": "true", "IS_DRAFT": "false", "PRODUCER_ACTION": "synchronize"}
            return run_step(self.create_body, {**base, **env}, CREATE_GH, tmp=tmp)

    def test_pass_through_titles(self):
        cases = [  # (case, env, title, warning)
            ("draft", {"IS_DRAFT": "true"}, "Draft: not reviewed", False),
            ("dependabot update", {"IS_EXCLUDED_DEPENDABOT": "true"}, "Dependabot update: not sent to Codex", False),
            ("gate verdict label", {"ALREADY_CLEAN": "true", "LABEL_ACTOR": "github-actions[bot]",
                                    "EARLIER_VERDICT": "true"},
             "Codex review clean (earlier verdict for this revision)", False),
            ("workflow label, no verdict check-run", {"ALREADY_CLEAN": "true", "LABEL_ACTOR": "github-actions[bot]"},
             "Codex review overridden by github-actions[bot] (codex-clean label)", True),
            ("human override", {"ALREADY_CLEAN": "true", "LABEL_ACTOR": "thagale"},
             "Codex review overridden by thagale (codex-clean label)", True),
            ("applier unknown", {"ALREADY_CLEAN": "true", "LABEL_ACTOR": ""},
             "Codex review overridden (codex-clean label; applier unknown)", True),
        ]
        for case, env, wanted, warning in cases:
            with self.subTest(case=case):
                proc, written, calls = self._create(**env)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                posts = [c for c in calls if f"repos/{GATE_REPO}/check-runs" in c]
                self.assertEqual(len(posts), 1, calls)
                self.assertIn(f"name={CHECK_NAME}", posts[0])
                self.assertIn("status=completed", posts[0])
                self.assertIn("conclusion=success", posts[0])
                self.assertEqual(arg_prefixed(posts[0], "output[title]="), [f"output[title]={wanted}"])
                self.assertEqual(len(arg_prefixed(posts[0], "output[summary]=")), 1)
                self.assertEqual("::warning::" in proc.stdout, warning, proc.stdout)
                self.assertIn("conclusion=success", output_lines(written))
        with self.subTest(case="all false: a real round"):
            proc, written, calls = self._create()
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            posts = [c for c in calls if f"repos/{GATE_REPO}/check-runs" in c]
            self.assertEqual(len(posts), 1, calls)
            self.assertIn("status=in_progress", posts[0])
            self.assertEqual(arg_prefixed(posts[0], "output[title]="), [])
            self.assertEqual(arg_prefixed(posts[0], "conclusion="), [])
            self.assertIn("conclusion=", output_lines(written))

    def test_no_app_token_fails_instead_of_waiting(self):
        # Codex P1, PR #327: a dependabot[bot]-triggered event on a PR that is not a Dependabot
        # update has no request PAT and no hagale-agent token, so an in_progress check-run made with
        # github.token could never be completed. The review path fails the gate visibly instead.
        request = gate_step("codex-review-request", sid="create-check")
        self.assertEqual(wf.field(wf.sub(request, "env"), "APP_TOKEN_MINTED"),
                         "${{ steps.app-token.outcome == 'success' }}")
        self.assertEqual(wf.field(gate_step("codex-review-request", name="Request a review"), "if"),
                         "steps.create-check.outputs.check_run_id != '' && steps.create-check.outputs.conclusion == ''")
        for minted in ("false", ""):
            with self.subTest(app_token_minted=minted):
                proc, written, calls = self._create(APP_TOKEN_MINTED=minted)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                posts = [c for c in calls if f"repos/{GATE_REPO}/check-runs" in c]
                self.assertEqual(len(posts), 1, calls)
                self.assertIn("status=completed", posts[0])
                self.assertIn("conclusion=failure", posts[0])
                self.assertEqual(arg_prefixed(posts[0], "output[title]="),
                                 ["output[title]=Codex review not requested: event triggered by dependabot[bot]"])
                self.assertIn("::warning::", proc.stdout)
                self.assertIn("conclusion=failure", output_lines(written))
        # A genuine Dependabot update has no app token either, and still passes through as before;
        # so do the other pass-throughs, which need no cross-run write.
        passes = [
            ("dependabot update", {"IS_EXCLUDED_DEPENDABOT": "true"}, "Dependabot update: not sent to Codex"),
            ("draft", {"IS_DRAFT": "true"}, "Draft: not reviewed"),
            ("label override", {"ALREADY_CLEAN": "true", "LABEL_ACTOR": "thagale"},
             "Codex review overridden by thagale (codex-clean label)"),
        ]
        for case, env, wanted in passes:
            with self.subTest(case=case):
                proc, written, calls = self._create(APP_TOKEN_MINTED="false", **env)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                posts = [c for c in calls if f"repos/{GATE_REPO}/check-runs" in c]
                self.assertEqual(len(posts), 1, calls)
                self.assertIn("conclusion=success", posts[0])
                self.assertEqual(arg_prefixed(posts[0], "output[title]="), [f"output[title]={wanted}"])

    def test_clean_verdict_title(self):
        with tempfile.TemporaryDirectory() as tmp:
            env = {"GH_TOKEN": "x", "DISPATCH_TOKEN": "", "REPO": GATE_REPO, "HEAD_SHA": HEAD_SHA,
                   "OUTCOME": "success", "DISPATCH": "false", "INCOMPLETE": ""}
            proc, _, calls = run_step(self.update_body, env, UPDATE_GH, tmp=tmp)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        patches = [c for c in calls if "PATCH" in c]
        self.assertEqual(len(patches), 1, calls)
        self.assertIn("conclusion=success", patches[0])
        self.assertEqual(arg_prefixed(patches[0], "output[title]="), ["output[title]=Codex review clean"])
        self.assertFalse(has_call(calls, "POST"), "DISPATCH=false must not dispatch ci-full.yml")


# The `rules` of ruleset 18658768 (main-protection), read with
# `gh api repos/HagaleTechnologies/manta/rulesets/18658768` on 2026-10-16.
LIVE_2026_10_16 = {"rules": [
    {"type": "deletion"},
    {"type": "non_fast_forward"},
    {"type": "required_linear_history"},
    {"type": "required_status_checks", "parameters": {
        "strict_required_status_checks_policy": True,
        "do_not_enforce_on_create": False,
        "required_status_checks": [
            {"context": "test (macos-latest)"},
            {"context": "test (ubuntu-latest)"},
            {"context": CHECK_NAME},
            {"context": "Supply Chain"},
        ]}},
    {"type": "merge_queue", "parameters": {
        "merge_method": "SQUASH", "max_entries_to_build": 1, "min_entries_to_merge": 1,
        "max_entries_to_merge": 1, "min_entries_to_merge_wait_minutes": 0,
        "grouping_strategy": "ALLGREEN", "check_response_timeout_minutes": 60}},
]}


def ruleset(change=None, add=()):
    """LIVE_2026_10_16 with `change` ({context: integration_id}) applied and `add` entries appended."""
    doc = json.loads(json.dumps(LIVE_2026_10_16))
    checks = next(r for r in doc["rules"] if r["type"] == "required_status_checks")
    entries = checks["parameters"]["required_status_checks"]
    for entry in entries:
        if entry["context"] in (change or {}):
            entry["integration_id"] = change[entry["context"]]
    entries.extend(add)
    return doc


class RequiredCheckSourcesTests(unittest.TestCase):
    """MAN-245 scenario 3: listing the required checks shows each one's allowed source, or the
    recorded decision not to pin it."""

    def run_script(self, doc=None, raw=None):
        stdin = io.StringIO(raw if raw is not None else json.dumps(doc))
        out, err = io.StringIO(), io.StringIO()
        code = sources.main(["required-check-sources.py"], stdin, out, err)
        return code, out.getvalue().splitlines(), err.getvalue()

    def line(self, lines, context):
        found = [l for l in lines if l.startswith(context + " ")]
        self.assertEqual(len(found), 1, lines)
        return found[0]

    def test_live_ruleset_as_read(self):
        code, lines, _ = self.run_script(LIVE_2026_10_16)
        self.assertEqual(code, 1)
        supply = self.line(lines, "Supply Chain")
        self.assertIn("PIN NOT APPLIED", supply)
        self.assertIn("GitHub Actions (15368)", supply)
        for context in ("test (macos-latest)", "test (ubuntu-latest)", CHECK_NAME):
            with self.subTest(context=context):
                line = self.line(lines, context)
                self.assertIn("any source", line)
                self.assertIn("not pinned:", line)

    def test_supply_chain_pinned_as_recorded(self):
        code, lines, _ = self.run_script(ruleset({"Supply Chain": 15368}))
        self.assertEqual(code, 0, lines)
        self.assertIn("pinned as recorded", self.line(lines, "Supply Chain"))
        self.assertIn("GitHub Actions (15368)", self.line(lines, "Supply Chain"))

    def test_supply_chain_pinned_to_the_wrong_app(self):
        code, lines, _ = self.run_script(ruleset({"Supply Chain": 4696353}))
        self.assertEqual(code, 1)
        line = self.line(lines, "Supply Chain")
        self.assertIn("hagale-agent (4696353)", line)
        self.assertIn("recorded GitHub Actions (15368)", line)

    def test_windows_added_unpinned(self):  # MAN-371 owner step O1
        code, lines, _ = self.run_script(ruleset({"Supply Chain": 15368}, [{"context": "test (windows-latest)"}]))
        self.assertEqual(code, 0, lines)
        self.assertIn("not pinned:", self.line(lines, "test (windows-latest)"))

    def test_pr_title_must_be_pinned(self):  # MAN-156 owner step O4
        code, lines, _ = self.run_script(ruleset({"Supply Chain": 15368}, [{"context": "PR title"}]))
        self.assertEqual(code, 1)
        self.assertIn("PIN NOT APPLIED", self.line(lines, "PR title"))
        code, lines, _ = self.run_script(
            ruleset({"Supply Chain": 15368}, [{"context": "PR title", "integration_id": 15368}]))
        self.assertEqual(code, 0, lines)

    def test_unknown_context_has_no_decision(self):
        code, lines, _ = self.run_script(ruleset({"Supply Chain": 15368}, [{"context": "Foo"}]))
        self.assertEqual(code, 1)
        self.assertIn("NO RECORDED DECISION", self.line(lines, "Foo"))

    def test_codex_check_pinned_against_the_record(self):
        for iid in (15368, 4696353):
            with self.subTest(integration_id=iid):
                code, lines, _ = self.run_script(ruleset({"Supply Chain": 15368, CHECK_NAME: iid}))
                self.assertEqual(code, 1)
                self.assertIn("BUT recorded not pinned:", self.line(lines, CHECK_NAME))

    def test_unusable_input_is_a_usage_error(self):
        no_checks = {"rules": [r for r in LIVE_2026_10_16["rules"] if r["type"] != "required_status_checks"]}
        for case, kwargs in (("no required_status_checks rule", {"doc": no_checks}),
                             ("invalid JSON", {"raw": "{not json"}),
                             ("not a ruleset", {"raw": "42"})):
            with self.subTest(case=case):
                code, lines, err = self.run_script(**kwargs)
                self.assertEqual(code, 2)
                self.assertEqual(lines, [])
                self.assertIn("usage:", err)

    def test_last_line_names_the_decision_record(self):
        _, lines, _ = self.run_script(LIVE_2026_10_16)
        self.assertEqual(lines[-1], f"decisions: {sources.DECISION_RECORD}")
        self.assertTrue(os.path.isfile(os.path.join(ROOT, sources.DECISION_RECORD)), sources.DECISION_RECORD)

    def test_decision_record_lists_every_recorded_check(self):
        with open(os.path.join(ROOT, sources.DECISION_RECORD), encoding="utf-8") as f:
            record = f.read()
        for context in sources.DECISIONS:
            with self.subTest(context=context):
                self.assertIn(f"`{context}`", record)

    def test_script_runs_as_a_command(self):
        proc = subprocess.run([os.path.join(ROOT, "scripts", "required-check-sources.py")],
                              input=json.dumps(LIVE_2026_10_16), capture_output=True, text=True)
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn("PIN NOT APPLIED", proc.stdout)


# A literal check-run name in a Checks API POST (`-f name="$name"` is a GraphQL repository name).
CHECK_POST_NAME = re.compile(r'-f name="([^"$][^"]*)"')


def workflow_texts():
    for path in sorted(glob.glob(os.path.join(ROOT, ".github", "workflows", "*.yml"))):
        yield os.path.basename(path), wf.read(os.path.basename(path))


class PinnedCheckProducerTests(unittest.TestCase):
    """A check pinned to one app must have exactly one producer on every path, or the pin stalls
    every merge: one native job of that name, and no Checks API POST of it."""

    def test_pinned_checks_have_one_native_producer(self):
        pinned = [c for c, d in sources.DECISIONS.items() if d[0] == "pin"]
        self.assertEqual(sorted(pinned), ["PR title", "Supply Chain"])
        jobs, posted = {}, set()
        for name, text in workflow_texts():
            for job in wf.job_ids(text):
                jobs.setdefault(wf.field(wf.job_block(text, job), "name") or job, []).append(f"{name}:{job}")
            for line in wf.code_lines(text):
                posted.update(CHECK_POST_NAME.findall(line))
        for context in pinned:
            with self.subTest(context=context):
                self.assertEqual(sources.DECISIONS[context][1], 15368)
                self.assertEqual(len(jobs.get(context, [])), 1, jobs.get(context))
                self.assertNotIn(context, posted)
        unpinned = {c for c, d in sources.DECISIONS.items() if d[0] == "unpinned"}
        self.assertEqual(posted, unpinned, "the Checks-API POST sites create exactly the unpinned checks")


class MainAlertWatchTests(unittest.TestCase):
    """MAN-241: main-alert.yml kept watching `Release` after release.yml was deleted. A
    workflow_run filter entry naming no workflow never fires, so every watched name must exist."""

    def test_every_watched_workflow_exists(self):
        names = set()
        for path in wf.workflow_files():
            m = re.search(r"^name:\s*(.+?)\s*$", wf.read(os.path.basename(path)), re.M)
            if m:
                names.add(m.group(1).strip("'\""))
        lines = wf.read("main-alert.yml").splitlines()
        trigger = wf.sub(lines[lines.index("on:"):], "workflow_run")
        self.assertIsNotNone(trigger, "main-alert.yml: no workflow_run trigger")
        workflows = wf.sub(trigger, "workflows")
        self.assertIsNotNone(workflows, "main-alert.yml: workflow_run has no workflows list")
        # code_lines drops comment lines, so an entry after one is still checked.
        watched = [line.strip()[2:].strip().strip("'\"")
                   for line in wf.code_lines(workflows[1:]) if line.strip().startswith("- ")]
        self.assertTrue(watched, "main-alert.yml: workflows is not a block list")
        self.assertEqual([w for w in watched if w not in names], [])


if __name__ == "__main__":
    unittest.main()
