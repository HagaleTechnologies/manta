"""Static checks for the MAN-243 CI trust boundary (which PR revisions may run in main's context,
which runs may write the shared build cache, and that release builds start cold), for MAN-245's
one Dependabot classification across the gate workflows, and for MAN-340's dispatched-tree merge
(a dispatched run tests the PR head merged with the main commit its workflow file came from).

stdlib unittest only -- PyYAML is not available on every runner -- so workflows are read as text.
Run: python3 -m unittest scripts.tests.test_ci_trust_boundary -v
"""
import glob
import json
import os
import re
import shlex
import subprocess
import tempfile
import textwrap
import unittest

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
WF = os.path.join(ROOT, ".github", "workflows")
REPO = "HagaleTechnologies/manta"
TRUSTED = {"thagale", "catalyst-cloud-connector[bot]"}
DISPATCH_CALL = "actions/workflows/ci-full.yml/dispatches"
BLOCK_SCALARS = {"|", "|-", "|+", ">", ">-", ">+"}
CI_TEST_JOBS = ("test", "test-soapy")
SAVE_IF = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}"
NATIVE_IF = ("github.event_name != 'pull_request' || "
             "github.event.pull_request.head.repo.full_name != github.repository || "
             "!contains(fromJSON('[\"thagale\",\"catalyst-cloud-connector[bot]\"]'), "
             "github.event.pull_request.user.login)")
# MAN-245: the one "Dependabot update" classification every gate workflow uses -- opened by
# dependabot[bot] from a branch in this repository, and this event triggered by dependabot[bot].
DEPENDABOT_UPDATE = ("github.actor == 'dependabot[bot]' && "
                     "github.event.pull_request.user.login == 'dependabot[bot]' && "
                     "github.event.pull_request.head.repo.full_name == github.repository")
DEPENDABOT = "dependabot[bot]"


def read(name):
    """Text of .github/workflows/<name>."""
    with open(os.path.join(WF, name), encoding="utf-8") as f:
        return f.read()


def code_lines(text):
    """Lines whose lstrip() does not start with '#' (drops YAML and shell comment lines)."""
    lines = text if isinstance(text, list) else text.splitlines()
    return [line for line in lines if not line.lstrip().startswith("#")]


def _indent(line):
    return len(line) - len(line.lstrip(" "))


def job_ids(text):
    """Job ids under the top-level `jobs:` key, in file order."""
    ids, inside = [], False
    for line in text.splitlines():
        if line == "jobs:":
            inside = True
            continue
        if inside:
            if re.match(r"^\S", line) and not line.startswith("#"):
                break
            m = re.match(r"^  ([A-Za-z0-9_-]+):\s*$", line)
            if m:
                ids.append(m.group(1))
    return ids


def job_block(text, job):
    """Lines from '  <job>:' up to the next 2-space-indented key (same rule as
    release-version.test.sh's job_block)."""
    out, inside = [], False
    for line in text.splitlines():
        if line == f"  {job}:":
            inside = True
            out.append(line)
            continue
        if inside and re.match(r"^  [A-Za-z0-9_-]+:", line):
            break
        if inside:
            out.append(line)
    return out


def steps(block):
    """The job's steps as line lists, split on the '- ' items under 'steps:'. Comment lines at
    the item indent sit between steps and belong to none of them."""
    try:
        start = next(i for i, line in enumerate(block) if line.strip() == "steps:")
    except StopIteration:
        return []
    item_indent, out, current = None, [], None
    for line in block[start + 1:]:
        if not line.strip():
            if current is not None:
                current.append(line)
            continue
        if item_indent is None:
            if line.lstrip().startswith("#"):
                continue
            item_indent = _indent(line)
        if _indent(line) < item_indent:
            break
        if _indent(line) == item_indent:
            if line.lstrip().startswith("#"):
                current = None
                continue
            if line.lstrip().startswith("- "):
                current = [line]
                out.append(current)
                continue
        if current is not None:
            current.append(line)
    return out


def _mapping(lines):
    """(lines, key indent). A step's '- ' marker is turned into spaces so all its keys sit at one
    indent; a header block ('  job:', '        env:') has its keys 2 deeper than the header."""
    first = lines[0]
    if first.lstrip().startswith("- "):
        i = _indent(first)
        return [first[:i] + "  " + first[i + 2:]] + list(lines[1:]), i + 2
    return list(lines), _indent(first) + 2


def _find(lines, key):
    """(normalised lines, index, raw value) of '<key>:' at the mapping's own key indent; the
    index and value are None when the key is absent."""
    lines, indent = _mapping(lines)
    pattern = re.compile(r"^ {%d}%s:(?:\s+(.*))?$" % (indent, re.escape(key)))
    for i, line in enumerate(lines):
        m = pattern.match(line)
        if m:
            return lines, i, (m.group(1) or "").strip()
    return lines, None, None


def _block_lines(lines, i):
    """The lines nested under lines[i] (deeper indent, or blank), trailing blanks dropped."""
    base = _indent(lines[i])
    out = []
    for line in lines[i + 1:]:
        if line.strip() and _indent(line) <= base:
            break
        out.append(line)
    while out and not out[-1].strip():
        out.pop()
    return out


def field(lines, key):
    """Scalar or folded ('>-' / '|') value of '<key>:' in a step, a job header block or a
    sub-mapping, whitespace-normalised to single spaces; None when the key is absent."""
    lines, i, value = _find(lines, key)
    if i is None:
        return None
    if value in BLOCK_SCALARS:
        value = " ".join(_block_lines(lines, i))
    return " ".join(value.split())


def sub(lines, key):
    """The '<key>:' header line plus everything nested under it, or None."""
    lines, i, _ = _find(lines, key)
    if i is None:
        return None
    return [lines[i]] + _block_lines(lines, i)


def run_body(step):
    """textwrap.dedent of the step's 'run: |' block."""
    lines, i, value = _find(step, "run")
    if i is None:
        return None
    if value not in BLOCK_SCALARS:
        return value + "\n"
    return textwrap.dedent("\n".join(_block_lines(lines, i))) + "\n"


def step_by_id(block, sid):
    """The step whose 'id:' is sid, or None."""
    for step in steps(block):
        if field(step, "id") == sid:
            return step
    return None


def trust_bodies():
    """{job: run body} for each wait-for-codex.yml job that has an `id: trust` step."""
    text = read("wait-for-codex.yml")
    out = {}
    for job in ("codex-review-request", "codex-review-response"):
        step = step_by_id(job_block(text, job), "trust")
        if step is not None:
            out[job] = run_body(step)
    return out


TRUST_CASES = [  # (label, HEAD_REPO, AUTHOR, IS_DRAFT, expected dispatch)
    ("trusted owner, ready", REPO, "thagale", "false", "true"),
    ("trusted agent bot, ready", REPO, "catalyst-cloud-connector[bot]", "false", "true"),
    ("trusted owner, draft", REPO, "thagale", "true", "false"),
    ("trusted owner, draft unknown", REPO, "thagale", "", "false"),
    ("fork head, trusted login", "someone/manta", "thagale", "false", "false"),
    ("fork head, outsider, draft", "someone/manta", "someone", "true", "false"),
    ("deleted fork (null head repo)", "", "someone", "false", "false"),
    ("in-repo head, untrusted app", REPO, "chatgpt-codex-connector[bot]", "false", "false"),
    ("dependabot", REPO, "dependabot[bot]", "false", "false"),
    ("glob look-alike login", REPO, "catalyst-cloud-connectorb", "false", "false"),
    ("case-variant head repo", "hagaletechnologies/manta", "thagale", "false", "false"),
]


class GateTrustTests(unittest.TestCase):
    def setUp(self):
        self.text = read("wait-for-codex.yml")

    def test_both_jobs_have_a_trust_step_with_identical_bodies(self):
        bodies = {}
        for job in ("codex-review-request", "codex-review-response"):
            matches = [s for s in steps(job_block(self.text, job)) if field(s, "id") == "trust"]
            self.assertEqual(len(matches), 1, f"{job}: expected exactly one step with id: trust")
            bodies[job] = run_body(matches[0])
            self.assertTrue(bodies[job] and bodies[job].strip(), f"{job}: trust step has no run: body")
        self.assertEqual(bodies["codex-review-request"], bodies["codex-review-response"],
                         "the two trust steps' run: bodies must be byte-identical")

    def test_trust_step_body_uses_env_not_expressions(self):
        bodies = trust_bodies()
        self.assertEqual(len(bodies), 2)
        for job, body in bodies.items():
            self.assertNotIn("${{", body, f"{job}: trust inputs must arrive via env:, not ${{{{ }}}}")

    def test_trust_decision_table(self):
        bodies = trust_bodies()
        self.assertEqual(len(bodies), 2)
        for job, body in bodies.items():
            with tempfile.TemporaryDirectory() as tmp:
                script = os.path.join(tmp, "trust.sh")
                with open(script, "w", encoding="utf-8") as f:
                    f.write(body)
                cwd = os.path.join(tmp, "cwd")
                os.mkdir(cwd)
                # An unquoted [bot] pattern would glob-match this file name.
                open(os.path.join(cwd, "catalyst-cloud-connectorb"), "w").close()
                for label, head_repo, author, is_draft, expected in TRUST_CASES:
                    with self.subTest(job=job, case=label):
                        out = os.path.join(tmp, f"out-{len(os.listdir(tmp))}")
                        open(out, "w").close()
                        env = {
                            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                            "GITHUB_OUTPUT": out,
                            "REPO": REPO,
                            "HEAD_REPO": head_repo,
                            "AUTHOR": author,
                            "IS_DRAFT": is_draft,
                        }
                        proc = subprocess.run(
                            ["bash", "--noprofile", "--norc", "-eo", "pipefail", script],
                            cwd=cwd, env=env, capture_output=True, text=True,
                        )
                        self.assertEqual(proc.returncode, 0, proc.stderr)
                        with open(out, encoding="utf-8") as f:
                            self.assertEqual(f.read().strip(), f"dispatch={expected}", proc.stdout)

    def test_every_ci_dispatch_is_gated_on_trust(self):
        dispatch_sites = 0
        for job in job_ids(self.text):
            job_steps = steps(job_block(self.text, job))
            trust_idx = next((i for i, s in enumerate(job_steps) if field(s, "id") == "trust"), None)
            for idx, step in enumerate(job_steps):
                if not any(DISPATCH_CALL in line for line in code_lines(step)):
                    continue
                dispatch_sites += 1
                with self.subTest(job=job, step=field(step, "name")):
                    self.assertIsNotNone(trust_idx, f"{job}: dispatches ci-full.yml with no trust step")
                    self.assertLess(trust_idx, idx, f"{job}: the trust step must precede the dispatch")
                    cond = field(step, "if") or ""
                    if "steps.trust.outputs.dispatch == 'true'" in cond:
                        continue
                    env = sub(step, "env")
                    self.assertIsNotNone(env, f"{job}: dispatch step is neither if:-gated nor env-gated")
                    self.assertEqual(field(env, "DISPATCH"), "${{ steps.trust.outputs.dispatch }}")
                    body = run_body(step)
                    guard = body.find('[ "$DISPATCH" != "true" ]')
                    call = body.find(DISPATCH_CALL)
                    self.assertNotEqual(guard, -1, f"{job}: no DISPATCH guard in the run: body")
                    self.assertLess(guard, call, f"{job}: the DISPATCH guard must come before the call")
                    guard_branch = body[guard:body.find("fi", guard)]
                    self.assertIn("exit 0", guard_branch, f"{job}: the DISPATCH guard must stop the step")
        self.assertEqual(dispatch_sites, 2, "expected exactly two ci-full.yml dispatch sites")

    def test_response_job_reads_trust_inputs_from_the_live_pr(self):
        block = job_block(self.text, "codex-review-response")
        body = run_body(step_by_id(block, "pr-context"))
        self.assertIn('gh api "repos/$REPO/pulls/$PR"', body)
        for selector in (".head.repo.full_name", ".user.login", ".draft"):
            self.assertIn(selector, body)
        self.assertIn('>> "$GITHUB_OUTPUT"', body)
        for output in ("head_repo=", "author=", "is_draft="):
            self.assertIn(output, body)
        env = sub(step_by_id(block, "trust"), "env")
        self.assertEqual(field(env, "REPO"), "${{ github.repository }}")
        self.assertEqual(field(env, "HEAD_REPO"), "${{ steps.pr-context.outputs.head_repo }}")
        self.assertEqual(field(env, "AUTHOR"), "${{ steps.pr-context.outputs.author }}")
        self.assertEqual(field(env, "IS_DRAFT"), "${{ steps.pr-context.outputs.is_draft }}")

    def test_producer_trust_inputs_come_from_the_event(self):
        env = sub(step_by_id(job_block(self.text, "codex-review-request"), "trust"), "env")
        self.assertEqual(field(env, "REPO"), "${{ github.repository }}")
        self.assertEqual(field(env, "HEAD_REPO"), "${{ github.event.pull_request.head.repo.full_name }}")
        self.assertEqual(field(env, "AUTHOR"), "${{ github.event.pull_request.user.login }}")
        self.assertEqual(field(env, "IS_DRAFT"), "${{ github.event.pull_request.draft }}")


class TrustedListTests(unittest.TestCase):
    def test_trusted_authors_identical_everywhere(self):
        auto_merge = set(re.findall(r"github\.event\.pull_request\.user\.login == '([^']+)'",
                                    "\n".join(code_lines(read("auto-merge-trigger.yml")))))
        self.assertEqual(auto_merge, TRUSTED, "auto-merge-trigger.yml")
        bodies = trust_bodies()
        self.assertEqual(len(bodies), 2)
        for job, body in bodies.items():
            m = re.search(r"^trusted_authors=\((.*)\)$", body, re.M)
            self.assertIsNotNone(m, f"wait-for-codex.yml {job}: no trusted_authors=( ... ) line")
            self.assertEqual(set(shlex.split(m.group(1))), TRUSTED, f"wait-for-codex.yml {job}")
        ci_full = read("ci-full.yml")
        for job in CI_TEST_JOBS:
            cond = field(job_block(ci_full, job), "if") or ""
            m = re.search(r"fromJSON\('(\[[^']*\])'\)", cond)
            self.assertIsNotNone(m, f"ci-full.yml {job}: no fromJSON('[...]') trusted list in if:")
            self.assertEqual(set(json.loads(m.group(1))), TRUSTED, f"ci-full.yml {job}")


def rust_cache_steps(text):
    """(job, step) for every Swatinem/rust-cache step in a workflow."""
    out = []
    for job in job_ids(text):
        for step in steps(job_block(text, job)):
            if (field(step, "uses") or "").startswith("Swatinem/rust-cache@"):
                out.append((job, step))
    return out


def workflow_files():
    return sorted(glob.glob(os.path.join(WF, "*.yml")) + glob.glob(os.path.join(WF, "*.yaml")))


CACHE_MARKERS = ("actions/cache@", "cache-from:", "cache-to:", "sccache")


class CiFullCacheTests(unittest.TestCase):
    def test_rust_cache_saves_only_on_push_to_main(self):
        found = rust_cache_steps(read("ci-full.yml"))
        self.assertEqual(len(found), 2, "expected exactly two rust-cache steps in ci-full.yml")
        for job, step in found:
            with self.subTest(job=job):
                with_block = sub(step, "with")
                self.assertIsNotNone(with_block, f"{job}: rust-cache step has no with: block")
                self.assertEqual(field(with_block, "save-if"), SAVE_IF)

    def test_only_ci_full_uses_a_build_cache(self):
        users = set()
        for path in workflow_files():
            name = os.path.basename(path)
            with open(path, encoding="utf-8") as f:
                code = "\n".join(code_lines(f.read()))
            if "Swatinem/rust-cache@" in code:
                users.add(name)
            for marker in CACHE_MARKERS:
                self.assertNotIn(marker, code, f"{name} uses {marker}")
        self.assertEqual(users, {"ci-full.yml"})


class CiFullNativePathTests(unittest.TestCase):
    def setUp(self):
        self.text = read("ci-full.yml")

    def test_hpsdr_coverage_moves_to_default_ci_and_catalyst_together(self):
        self.assertEqual(tuple(j for j in job_ids(self.text) if j.startswith("test")), CI_TEST_JOBS)
        # This suite also runs on macOS; do not require Python 3.11's tomllib.
        with open(os.path.join(ROOT, ".catalyst", "catalyst.toml"), encoding="utf-8") as f:
            config = f.read()
        names = set(re.findall(r'^name = "([^"\n]+)"$', config, re.M))
        self.assertFalse({"clippy-hpsdr", "test-hpsdr"} & names)
        command = re.search(r'^name = "test-workspace"\nrun = (\[[^\n]*\])$', config, re.M)
        self.assertIsNotNone(command, "Catalyst must retain the workspace test gate")
        self.assertEqual(json.loads(command.group(1)), ["cargo", "test", "--workspace", "--locked"])
        self.assertTrue(any((run_body(s) or "").strip() == "cargo test --workspace --locked"
                            for s in steps(job_block(self.text, "test"))))

    def test_test_jobs_run_natively_for_non_dispatch_eligible_prs(self):
        for job in CI_TEST_JOBS:
            with self.subTest(job=job):
                self.assertEqual(field(job_block(self.text, job), "if"), NATIVE_IF)

    def test_pull_request_trigger_types_unchanged(self):
        m = re.search(r"^on:\n  pull_request:\n    types: \[([^\]]*)\]$", self.text, re.M)
        self.assertIsNotNone(m, "on.pull_request.types not found")
        self.assertEqual([t.strip() for t in m.group(1).split(",")],
                         ["opened", "reopened", "synchronize", "ready_for_review"])

    def test_trust_boundary_test_runs_in_required_test_job(self):
        wanted = "python3 -m unittest scripts.tests.test_ci_trust_boundary -v"
        matches = [s for s in steps(job_block(self.text, "test")) if field(s, "run") == wanted]
        self.assertEqual(len(matches), 1, f"job test has no step running {wanted!r}")
        self.assertEqual(field(matches[0], "if"), "runner.os != 'Windows'")


# MAN-340: a dispatched run merges github.sha -- the main commit its workflow file came from --
# into the PR head before any other step of ci-full.yml's test jobs.
MERGE_STEP_ID = "merge-main"
MERGE_STEP_NAME = "Merge the main commit this workflow came from into the PR head"
DISPATCH_ONLY = "github.event_name == 'workflow_dispatch'"
FETCH_DEPTH = "${{ github.event_name != 'workflow_dispatch' && 1 || 0 }}"
MERGE_ENV = {"HEAD_SHA": "${{ inputs.head_sha || github.sha }}", "BASE_SHA": "${{ github.sha }}"}
CHECKOUT_REF = ("${{ github.event_name == 'workflow_dispatch' && "
                "(inputs.head_sha || github.sha) || github.sha }}")


def gh_truthy(value):
    """GitHub expression truthiness: false, 0, -0, '' and null are falsy."""
    return value not in (False, 0, "", None)


def gh_and(a, b):
    return b if gh_truthy(a) else a


def gh_or(a, b):
    return a if gh_truthy(a) else b


def fetch_depth(event_name):
    """Python mirror of FETCH_DEPTH (tied to the YAML text by CiFullDispatchMergeWiringTests)."""
    return gh_or(gh_and(event_name != "workflow_dispatch", 1), 0)


def test_job_head(job):
    """(checkout step, step after it) for one of ci-full.yml's test jobs."""
    job_steps = steps(job_block(read("ci-full.yml"), job))
    return job_steps[0], (job_steps[1] if len(job_steps) > 1 else None)


def merge_step_body():
    """The run: body of the merge-main step in ci-full.yml's `test` job, or None."""
    step = step_by_id(job_block(read("ci-full.yml"), "test"), MERGE_STEP_ID)
    return None if step is None else run_body(step)


def scrubbed_git_env(home):
    """os.environ with no ambient git configuration: the container's GIT_CONFIG_KEY_n entries,
    the system and global config files and any GIT_DIR-style redirection are all dropped."""
    env = dict(os.environ, GIT_CONFIG_COUNT="0", GIT_CONFIG_NOSYSTEM="1",
               GIT_CONFIG_GLOBAL=os.devnull, HOME=home, PYTHONDONTWRITEBYTECODE="1")
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        env.pop(key, None)
    return env


class CiFullDispatchMergeWiringTests(unittest.TestCase):
    """MAN-340: where the merge-main step sits, when it runs and what it may touch."""

    def setUp(self):
        self.text = read("ci-full.yml")

    def test_merge_step_follows_checkout_in_every_test_job(self):
        for job in CI_TEST_JOBS:
            with self.subTest(job=job):
                checkout, step = test_job_head(job)
                self.assertTrue((field(checkout, "uses") or "").startswith("actions/checkout@"),
                                f"{job}: the first step is not actions/checkout")
                self.assertIsNotNone(step, f"{job}: no merge-main step after checkout")
                self.assertEqual(field(step, "id"), MERGE_STEP_ID, f"{job}: no merge-main step after checkout")
                self.assertEqual(field(step, "if"), DISPATCH_ONLY)
                self.assertEqual(field(step, "shell"), "bash")
                self.assertEqual(field(step, "name"), MERGE_STEP_NAME)
                env = sub(step, "env")
                self.assertIsNotNone(env, f"{job}: the merge-main step has no env: block")
                for key, value in MERGE_ENV.items():
                    self.assertEqual(field(env, key), value, f"{job}: env {key}")
                keys = [line for line in code_lines(env[1:]) if line.strip()]
                self.assertEqual(len(keys), len(MERGE_ENV), f"{job}: unexpected env keys {keys}")

    def test_checkout_fetches_full_history_only_on_dispatch(self):
        for job in CI_TEST_JOBS:
            with self.subTest(job=job):
                checkout, _ = test_job_head(job)
                with_block = sub(checkout, "with")
                self.assertIsNotNone(with_block, f"{job}: checkout has no with: block")
                self.assertEqual(field(with_block, "fetch-depth"), FETCH_DEPTH)
                self.assertEqual(field(with_block, "ref"), CHECKOUT_REF)
                self.assertEqual(field(with_block, "persist-credentials"), "false")
        self.assertEqual(fetch_depth("workflow_dispatch"), 0)
        for event in ("pull_request", "push", "merge_group"):
            with self.subTest(event=event):
                self.assertEqual(fetch_depth(event), 1)
        # The form not to use: `cond && 0 || 1` is 1 whatever cond is, because 0 is falsy.
        self.assertEqual(gh_or(gh_and(True, 0), 1), 1)

    def test_merge_step_bodies_are_byte_identical(self):
        bodies = {}
        for job in CI_TEST_JOBS:
            step = step_by_id(job_block(self.text, job), MERGE_STEP_ID)
            self.assertIsNotNone(step, f"{job}: no merge-main step")
            bodies[job] = run_body(step)
            self.assertTrue(bodies[job] and bodies[job].strip(), f"{job}: merge-main has no run: body")
        self.assertEqual(bodies["test"], bodies["test-soapy"],
                         "the two merge-main steps' run: bodies must be byte-identical")

    def test_merge_step_holds_no_credentials(self):
        for job in CI_TEST_JOBS:
            with self.subTest(job=job):
                block = job_block(self.text, job)
                step = step_by_id(block, MERGE_STEP_ID)
                self.assertIsNotNone(step, f"{job}: no merge-main step")
                body = run_body(step)
                for needle in ("${{", "secrets.", "github.token", "GH_TOKEN", "git push",
                               "git fetch", "git remote", "scripts/"):
                    self.assertNotIn(needle, body, f"{job}: merge-main body contains {needle!r}")
                self.assertIsNone(sub(step, "with"), f"{job}: merge-main has a with: block")
                self.assertIsNone(sub(step, "permissions"), f"{job}: merge-main declares permissions")
                self.assertIsNone(field(block, "permissions"), f"{job}: job-level permissions added")
        self.assertIn("\npermissions:\n  contents: read\n", self.text)

    def test_native_and_merged_runs_never_merge(self):
        for job in CI_TEST_JOBS:
            with self.subTest(job=job):
                step = step_by_id(job_block(self.text, job), MERGE_STEP_ID)
                self.assertIsNotNone(step, f"{job}: no merge-main step")
                cond = field(step, "if")
                self.assertEqual(cond, DISPATCH_ONLY)
                for event in ("pull_request", "push", "merge_group"):
                    self.assertNotIn(event, cond, f"{job}: merge-main runs on {event}")


TEST_PACKAGING = textwrap.dedent("""\
    import unittest


    class Packaging(unittest.TestCase):
        def test_ok(self):
            self.assertTrue(True)
""")
MISSING_MODULE = "No module named 'scripts.tests.test_packaging'"


class CiFullDispatchMergeBehaviourTests(unittest.TestCase):
    """MAN-340: the real merge-main body from ci-full.yml, run against throwaway repositories
    shaped like the ticket's scenario (a PR cut before main gained a test module)."""

    def setUp(self):
        self.body = merge_step_body()
        self.assertIsNotNone(self.body, "no merge-main step in ci-full.yml's test job")
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = tmp.name
        self.env = scrubbed_git_env(self.tmp)
        self.script = os.path.join(self.tmp, "merge-main.sh")
        with open(self.script, "w", encoding="utf-8") as f:
            f.write(self.body)

    def git(self, cwd, *args, check=True):
        proc = subprocess.run(
            ["git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid", *args],
            cwd=cwd, env=self.env, capture_output=True, text=True,
        )
        if check:
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        return proc

    def rev(self, cwd, ref):
        return self.git(cwd, "rev-parse", ref).stdout.strip()

    def _write(self, root, edits):
        for path, content in edits.items():
            full = os.path.join(root, path)
            if content is None:
                self.git(root, "rm", "-q", path)
                continue
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(content)
            self.git(root, "add", path)

    def _commit(self, root, edits, message):
        self._write(root, edits)
        self.git(root, "commit", "-q", "-m", message)
        return self.rev(root, "HEAD")

    def _fixture(self, pr_edits=None, main_edits=None):
        """(work, a, pr, b): commit A, a `pr` branch from A (the PR's own code), main's B on A
        adding scripts/tests/test_packaging.py, and `work`, a full clone at the PR head."""
        src = os.path.join(self.tmp, "src")
        os.mkdir(src)
        self.git(src, "init", "-q", "-b", "main")
        a = self._commit(src, {"scripts/__init__.py": "", "scripts/tests/__init__.py": "",
                               "shared.txt": "base\n"}, "A")
        self.git(src, "checkout", "-q", "-b", "pr")
        pr = self._commit(src, dict({"pr_change.txt": "the PR's own code\n"}, **(pr_edits or {})), "PR")
        self.git(src, "checkout", "-q", "main")
        b = self._commit(src, dict({"scripts/tests/test_packaging.py": TEST_PACKAGING},
                                   **(main_edits or {})), "B")
        self.git(self.tmp, "clone", "-q", "--no-checkout", src, "work")
        work = os.path.join(self.tmp, "work")
        self.git(work, "checkout", "-q", "--detach", pr)
        return work, a, pr, b

    def run_step(self, cwd, head_sha, base_sha):
        return subprocess.run(
            ["bash", "--noprofile", "--norc", "-eo", "pipefail", self.script],
            cwd=cwd, env=dict(self.env, HEAD_SHA=head_sha, BASE_SHA=base_sha),
            capture_output=True, text=True,
        )

    def unittest_packaging(self, cwd):
        return subprocess.run(
            ["python3", "-m", "unittest", "scripts.tests.test_packaging"],
            cwd=cwd, env=self.env, capture_output=True, text=True,
        )

    def test_a_pr_cut_before_a_new_test_module_is_judged_with_it(self):
        work, _, pr, b = self._fixture()
        before = self.unittest_packaging(work)
        self.assertNotEqual(before.returncode, 0, "the fixture no longer reproduces the bug")
        self.assertIn(MISSING_MODULE, before.stderr)
        proc = self.run_step(work, pr, b)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.rev(work, "HEAD^1"), pr)
        self.assertEqual(self.rev(work, "HEAD^2"), b)
        after = self.unittest_packaging(work)
        self.assertEqual(after.returncode, 0, after.stdout + after.stderr)
        self.assertNotIn("No module named", after.stderr)
        self.assertIn("::notice title=Tested with main merged in::", proc.stdout)

    def test_the_merged_tree_keeps_the_prs_own_changes(self):
        work, _, pr, b = self._fixture()
        proc = self.run_step(work, pr, b)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.git(work, "diff", "--name-only", b, "HEAD").stdout.split(), ["pr_change.txt"])
        self.assertEqual(self.git(work, "show", "HEAD:pr_change.txt").stdout,
                         self.git(work, "show", f"{pr}:pr_change.txt").stdout)

    def test_a_pr_that_deletes_a_needed_file_still_fails_on_its_own_change(self):
        work, _, pr, b = self._fixture(pr_edits={"shared.txt": None})
        proc = self.run_step(work, pr, b)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertFalse(os.path.exists(os.path.join(work, "shared.txt")),
                         "the merge restored a file the PR deleted")

    def test_a_head_that_already_contains_main_is_tested_as_is(self):
        work, _, _, b = self._fixture()
        self.git(work, "checkout", "-q", "--detach", b)
        c = self._commit(work, {"rebased.txt": "rebased onto main\n"}, "C")
        proc = self.run_step(work, c, b)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.rev(work, "HEAD"), c)
        self.assertIn("already contains", proc.stdout)

    def test_a_manual_dispatch_without_head_sha_is_a_no_op(self):
        work, _, _, b = self._fixture()
        self.git(work, "checkout", "-q", "--detach", b)
        proc = self.run_step(work, b, b)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.rev(work, "HEAD"), b)

    def test_a_conflict_fails_fast_naming_the_files(self):
        work, _, pr, b = self._fixture(pr_edits={"shared.txt": "pr\n"}, main_edits={"shared.txt": "main\n"})
        proc = self.run_step(work, pr, b)
        self.assertNotEqual(proc.returncode, 0, proc.stdout)
        self.assertIn("::error title=PR head conflicts with main::", proc.stdout)
        self.assertIn("shared.txt", proc.stdout)
        self.assertEqual(self.rev(work, "HEAD"), pr)
        self.assertNotEqual(self.git(work, "rev-parse", "-q", "--verify", "MERGE_HEAD", check=False).returncode, 0,
                            "the conflicted merge was not aborted")

    def test_a_shallow_checkout_fails_closed(self):
        _, _, pr, b = self._fixture()
        src = os.path.join(self.tmp, "src")
        self.git(self.tmp, "clone", "-q", "--depth=1", "--branch", "pr", "file://" + src, "shallow")
        proc = self.run_step(os.path.join(self.tmp, "shallow"), pr, b)
        self.assertNotEqual(proc.returncode, 0, proc.stdout)
        self.assertIn("::error title=Shallow checkout::", proc.stdout)

    def test_an_unexpected_checkout_fails_closed(self):
        work, _, _, b = self._fixture()
        proc = self.run_step(work, b, b)
        self.assertNotEqual(proc.returncode, 0, proc.stdout)
        self.assertIn("::error title=Unexpected checkout::", proc.stdout)

    def test_a_missing_main_commit_fails_closed(self):
        work, _, pr, _ = self._fixture()
        proc = self.run_step(work, pr, "f" * 40)
        self.assertNotEqual(proc.returncode, 0, proc.stdout)
        self.assertIn("::error title=Default-branch commit missing::", proc.stdout)



def is_dependabot_update(actor, author, head_repo):
    """Python mirror of DEPENDABOT_UPDATE (tied to the YAML text by DependabotClassificationTests)."""
    return actor == DEPENDABOT and author == DEPENDABOT and head_repo == REPO


def runs_natively(author, head_repo):
    """Python mirror of NATIVE_IF for a pull_request event."""
    return head_repo != REPO or author not in TRUSTED


DEPENDABOT_CASES = [  # (case, actor, author, head repo, Dependabot update?, ci-full native run?)
    ("1 genuine update", DEPENDABOT, DEPENDABOT, REPO, True, True),
    ("2 human pushes to bot PR", "thagale", DEPENDABOT, REPO, False, True),
    ("3 owner PR", "thagale", "thagale", REPO, False, False),
    ("4 agent PR", "catalyst-cloud-connector[bot]", "catalyst-cloud-connector[bot]", REPO, False, False),
    ("5 outside contributor, in-repo branch", "someuser", "someuser", REPO, False, True),
    ("6 owner event on contributor PR", "thagale", "someuser", REPO, False, True),
    ("7 fork", "someuser", "someuser", "someuser/manta", False, True),
    ("8 bot actor on human PR", DEPENDABOT, "thagale", REPO, False, False),
    ("9 bot login from a fork", DEPENDABOT, DEPENDABOT, "someuser/manta", False, True),
    ("10 deleted head repo", DEPENDABOT, DEPENDABOT, "", False, True),
]


class DependabotClassificationTests(unittest.TestCase):
    """MAN-245 scenario 2: a PR not authored by Dependabot from this repository takes the normal
    review path in every gate workflow, whoever triggered the event."""

    def test_codex_gate_excludes_only_dependabot_updates(self):
        step = step_by_id(job_block(read("wait-for-codex.yml"), "codex-review-request"), "create-check")
        self.assertEqual(field(sub(step, "env"), "IS_EXCLUDED_DEPENDABOT"), "${{ " + DEPENDABOT_UPDATE + " }}")

    def test_auto_merge_jobs_run_only_for_dependabot_updates(self):
        text = read("dependabot-auto-merge.yml")
        for job in ("verify", "check-needs-human", "auto-merge"):
            with self.subTest(job=job):
                self.assertEqual(field(job_block(text, job), "if"), DEPENDABOT_UPDATE)

    def test_no_actor_only_dependabot_test(self):
        negated = []
        for path in workflow_files():
            name = os.path.basename(path)
            with open(path, encoding="utf-8") as f:
                lines = code_lines(f.read())
            for line in lines:
                if "github.actor == 'dependabot[bot]'" in line:
                    with self.subTest(file=name, line=line.strip()):
                        self.assertIn("github.event.pull_request.user.login == 'dependabot[bot]'", line)
                if "github.actor != 'dependabot[bot]'" in line:
                    negated.append((name, line.strip()))
        # The one actor-only test left is a capability check, not a classification: GitHub
        # withholds Actions secrets from a Dependabot-actor run, so the app-token mint is skipped.
        self.assertEqual(negated, [("wait-for-codex.yml", "if: github.actor != 'dependabot[bot]'")])
        mint = step_by_id(job_block(read("wait-for-codex.yml"), "codex-review-request"), "app-token")
        self.assertEqual(field(mint, "if"), "github.actor != 'dependabot[bot]'")

    def test_classification_table(self):
        for case, actor, author, head_repo, update, native in DEPENDABOT_CASES:
            with self.subTest(case=case):
                self.assertEqual(is_dependabot_update(actor, author, head_repo), update)
                self.assertEqual(runs_natively(author, head_repo), native)


def release_workflows():
    return sorted(glob.glob(os.path.join(WF, "release*.yml")) + glob.glob(os.path.join(WF, "release*.yaml")))


class ReleaseColdBuildTests(unittest.TestCase):
    def test_release_workflows_restore_no_cache(self):
        files = release_workflows()
        self.assertTrue(files, "no .github/workflows/release*.yml found")
        for path in files:
            with open(path, encoding="utf-8") as f:
                code = "\n".join(code_lines(f.read()))
            for marker in ("Swatinem/rust-cache", "actions/cache", "cache-from", "cache-to", "sccache"):
                with self.subTest(file=os.path.basename(path), marker=marker):
                    self.assertNotIn(marker, code)

    def test_release_build_still_present(self):
        code = ""
        for path in release_workflows():
            with open(path, encoding="utf-8") as f:
                code += "\n".join(code_lines(f.read()))
        self.assertTrue("cargo build --release" in code or "cross build --release" in code,
                        "no release*.yml builds a release binary any more -- re-point this test")


DECISION_PATH_RE = re.compile(r"docs/DECISIONS/[0-9]{4}-[0-9]{2}-[0-9]{2}-man243-[A-Za-z0-9._-]*?\.md")
DOC_PATH_RE = re.compile(r"docs/[A-Za-z0-9/._-]*?man243[A-Za-z0-9._-]*?\.md")
RUNBOOK_PATH = "docs/RUNBOOKS/ci-trust-boundary.md"
MAN340_DECISION_RE = re.compile(r"docs/DECISIONS/[0-9]{4}-[0-9]{2}-[0-9]{2}-man340-[A-Za-z0-9._-]*?\.md")


class DocsReferenceTests(unittest.TestCase):
    def test_man243_doc_references_exist(self):
        files = ["wait-for-codex.yml", "ci-full.yml"] + [os.path.basename(p) for p in release_workflows()]
        for name in files:
            text = read(name)
            with self.subTest(file=name):
                self.assertTrue(DECISION_PATH_RE.search(text), f"{name} never names the MAN-243 decision record")
                refs = set(DOC_PATH_RE.findall(text))
                if RUNBOOK_PATH in text:
                    refs.add(RUNBOOK_PATH)
                for ref in sorted(refs):
                    self.assertTrue(os.path.isfile(os.path.join(ROOT, ref)), f"{name} names missing {ref}")

    def test_man340_decision_record_is_named_and_exists(self):
        refs = set(MAN340_DECISION_RE.findall(read("ci-full.yml")))
        self.assertTrue(refs, "ci-full.yml never names the MAN-340 decision record")
        for ref in sorted(refs):
            self.assertTrue(os.path.isfile(os.path.join(ROOT, ref)), f"ci-full.yml names missing {ref}")

    def test_runbook_explains_a_conflicted_dispatch(self):
        with open(os.path.join(ROOT, RUNBOOK_PATH), encoding="utf-8") as f:
            runbook = f.read()
        self.assertIn("PR head conflicts with main", runbook)
        self.assertTrue(MAN340_DECISION_RE.search(runbook), "the runbook never names the MAN-340 record")


if __name__ == "__main__":
    unittest.main()
