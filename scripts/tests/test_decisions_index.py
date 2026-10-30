"""MAN-440: the generated docs/DECISIONS/README.md index (scripts/decisions-index.py), the
committed index under the lag-tolerant check a record PR's own CI runs, two record-only PRs
merging in either order (the ticket's Scenario 1), and the Decisions Index workflow's shape and
behaviour (.github/workflows/decisions-index.yml).

stdlib unittest only -- workflows are read as text with test_ci_trust_boundary.py's helpers, and
run: bodies run under test_ci_gates.py's stub-gh harness.
Run: python3 -m unittest scripts.tests.test_decisions_index -v
"""
import base64
import contextlib
import glob
import importlib.util
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
SCRIPT = os.path.join(ROOT, "scripts", "decisions-index.py")
DECISIONS = os.path.join(ROOT, "docs", "DECISIONS")
WORKFLOW = "decisions-index.yml"
TEST_COMMAND = "python3 -m unittest scripts.tests.test_decisions_index -v"
TITLE = "docs(decisions): refresh the decision-record index"
BRANCH = "automation/decisions-index"
REPO = "HagaleTechnologies/manta"
BASE_SHA = "b" * 40
OLD_BLOB = "c" * 40
MAN440_RECORD = re.compile(r"[0-9]{4}-[0-9]{2}-[0-9]{2}-man440-[a-z0-9-]+\.md")


def _load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


di = _load("decisions_index", SCRIPT)
wf = _load("_ci_workflow_text", os.path.join(HERE, "test_ci_trust_boundary.py"))
gates = _load("_ci_gates", os.path.join(HERE, "test_ci_gates.py"))
title_rule = _load("check_pr_title", os.path.join(ROOT, "scripts", "check-pr-title.py"))

SKELETON = f"# Decision records\n\nHand-written preamble.\n\n{di.BEGIN}\n\n{di.END}\n\nTrailing text.\n"


def record_text(heading):
    return f"# {heading}\n\nDate: whenever.\n"


def write_records(directory, records):
    """records: {file name: heading}."""
    os.makedirs(directory, exist_ok=True)
    for name, heading in records.items():
        with open(os.path.join(directory, name), "w", encoding="utf-8") as f:
            f.write(record_text(heading))


def parsed(records):
    return sorted((di.parse_record(n, record_text(h)) for n, h in records.items()), key=di.Record.sort_key)


def generated(records):
    return di.replace_block(SKELETON, di.render_block(records))


def run_cli(*args):
    return subprocess.run([sys.executable, SCRIPT, *args], capture_output=True, text=True)


THREE = {
    "2026-10-27-man5-a.md": "MAN-5: a",
    "2026-10-28-man1-b.md": "MAN-1: b",
    "2026-10-26-man40-c.md": "MAN-40: c",
}


class ParseTests(unittest.TestCase):
    def test_name_gives_date_and_ticket(self):
        r = di.parse_record("2026-10-26-man436-x.md", record_text("MAN-436: x"))
        self.assertEqual((r.date, r.ticket, r.filename, r.title), ("2026-10-26", 436, "2026-10-26-man436-x.md", "MAN-436: x"))
        self.assertEqual(r.row(), "| 2026-10-26 | MAN-436 | [MAN-436: x](2026-10-26-man436-x.md) |")

    def test_man_dash_form(self):
        self.assertEqual(di.parse_record("2026-09-05-man-34-x.md", record_text("x")).ticket, 34)

    def test_ticketless_name_uses_first_heading_ticket(self):
        r = di.parse_record("2026-09-03-cross-source-spike.md", record_text("Spike (MAN-53), see MAN-60"))
        self.assertEqual(r.ticket, 53)
        self.assertIn("| MAN-53 |", r.row())

    def test_ticketless_name_and_heading_is_dash(self):
        r = di.parse_record("2026-09-01-rename-to-manta.md", record_text("Rename skimmer → manta"))
        self.assertIsNone(r.ticket)
        self.assertEqual(r.row(), "| 2026-09-01 | — | [Rename skimmer → manta](2026-09-01-rename-to-manta.md) |")

    def test_name_ticket_wins_over_the_heading(self):
        self.assertEqual(di.parse_record("2026-10-23-man179-x.md", record_text("MAN-179/MAN-166: x")).ticket, 179)

    def test_leading_date_dropped_from_title(self):
        for dash in ("—", "–", "-"):
            with self.subTest(dash=dash):
                r = di.parse_record("2026-10-20-man340-x.md", record_text(f"2026-10-20 {dash} MAN-340: x"))
                self.assertEqual(r.title, "MAN-340: x")

    def test_pipe_in_title_is_escaped(self):
        r = di.parse_record("2026-10-20-man1-x.md", record_text("a | b"))
        self.assertEqual(r.title, "a \\| b")
        self.assertEqual(re.split(r"(?<!\\)\|", r.row()).__len__(), 5, "the row must have exactly three cells")

    def test_bad_name_is_an_error(self):
        for name in ("2026-10-26-man436-X.md", "man436-x.md", "2026-13-40-man1-x.md",
                     "2026-02-30-man1-x.md", "2026-10-26-man436-x.txt", "2026-10-26.md"):
            with self.subTest(name=name):
                with self.assertRaises(di.RecordError) as cm:
                    di.parse_record(name, record_text("x"))
                self.assertTrue(str(cm.exception).startswith(f"docs/DECISIONS/{name}: "), str(cm.exception))

    def test_line_one_must_be_a_heading(self):
        for text in ("Title\n", "## Sub\n", "#\n", "# \n", "", "\n# late\n"):
            with self.subTest(text=text):
                with self.assertRaises(di.RecordError) as cm:
                    di.parse_record("2026-10-20-man1-x.md", text)
                self.assertIn("docs/DECISIONS/2026-10-20-man1-x.md: line 1", str(cm.exception))

    def test_wrapped_heading_is_an_error(self):
        with self.assertRaises(di.RecordError) as cm:
            di.parse_record("2026-10-23-man120-x.md", "# MAN-120: a heading that\nwraps\n\nbody\n")
        self.assertIn("docs/DECISIONS/2026-10-23-man120-x.md: line 2 must be blank", str(cm.exception))
        self.assertEqual(di.parse_record("2026-10-23-man120-x.md", "# one line").title, "one line")
        self.assertEqual(di.parse_record("2026-10-23-man120-x.md", "# crlf\r\n\r\nbody\r\n").title, "crlf")


class OrderTests(unittest.TestCase):
    def test_newest_first_then_ticket_desc_then_ticketless_then_name(self):
        with tempfile.TemporaryDirectory() as tmp:
            write_records(tmp, {
                "2026-10-27-man5-a.md": "MAN-5: a",
                "2026-10-28-man1-b.md": "MAN-1: b",
                "2026-10-27-man40-c.md": "MAN-40: c",
                "2026-10-27-man40-a.md": "MAN-40: a",
                "2026-10-27-z-ticketless.md": "z",
                "2026-10-27-a-ticketless.md": "a",
                "2026-10-27-spike.md": "spike (MAN-7)",
                "README.md": "# not a record",
            })
            with open(os.path.join(tmp, "notes.txt"), "w") as f:
                f.write("not markdown\n")
            os.mkdir(os.path.join(tmp, "sub.md"))
            names = [r.filename for r in di.load_records(tmp)]
        self.assertEqual(names, [
            "2026-10-28-man1-b.md",
            "2026-10-27-man40-a.md",
            "2026-10-27-man40-c.md",
            "2026-10-27-spike.md",
            "2026-10-27-man5-a.md",
            "2026-10-27-a-ticketless.md",
            "2026-10-27-z-ticketless.md",
        ])


class BlockTests(unittest.TestCase):
    def test_write_replaces_only_the_marked_block(self):
        readme = SKELETON.replace(f"{di.BEGIN}\n\n", f"{di.BEGIN}\n\n| stale |\n\n")
        block = di.render_block(parsed(THREE))
        out = di.replace_block(readme, block)
        head, tail = SKELETON.split(di.BEGIN)
        self.assertTrue(out.startswith(head + block))
        self.assertTrue(out.endswith(tail.split(di.END)[1]))
        self.assertNotIn("stale", out)
        self.assertEqual(block.split("\n")[:4], [di.BEGIN, "", di.HEADER, di.SEPARATOR])
        self.assertEqual(block.split("\n")[-2:], ["", di.END])

    def test_write_is_idempotent(self):
        once = generated(parsed(THREE))
        self.assertEqual(di.replace_block(once, di.render_block(parsed(THREE))), once)
        with tempfile.TemporaryDirectory() as tmp:
            write_records(tmp, THREE)
            with open(os.path.join(tmp, "README.md"), "w", encoding="utf-8") as f:
                f.write(SKELETON)
            self.assertEqual(run_cli("--write", "--dir", tmp).returncode, 0)
            with open(os.path.join(tmp, "README.md"), "rb") as f:
                first = f.read()
            self.assertEqual(run_cli("--write", "--dir", tmp).returncode, 0)
            with open(os.path.join(tmp, "README.md"), "rb") as f:
                self.assertEqual(f.read(), first)

    def test_missing_or_duplicated_markers_is_an_error(self):
        for readme in (SKELETON.replace(di.BEGIN, ""), SKELETON.replace(di.END, ""),
                       SKELETON.replace(di.BEGIN, f"{di.BEGIN}\n{di.BEGIN}"),
                       SKELETON.replace(di.END, f"{di.END}\n{di.END}"),
                       f"{di.END}\n{di.BEGIN}\n", "no markers at all\n"):
            with self.subTest(readme=readme):
                with self.assertRaises(di.RecordError):
                    di.replace_block(readme, di.render_block([]))
                with self.assertRaises(di.RecordError):
                    di.check(readme, [], strict=False)

    def test_output_is_lf_even_from_a_crlf_readme(self):
        with tempfile.TemporaryDirectory() as tmp:
            write_records(tmp, THREE)
            path = os.path.join(tmp, "README.md")
            with open(path, "w", encoding="utf-8", newline="\r\n") as f:
                f.write(generated(parsed(THREE)))
            self.assertEqual(run_cli("--strict", "--dir", tmp).returncode, 0)
            self.assertEqual(run_cli("--write", "--dir", tmp).returncode, 0)
            with open(path, "rb") as f:
                data = f.read()
        self.assertNotIn(b"\r", data)
        self.assertEqual(data.decode("utf-8"), generated(parsed(THREE)))


class ListedCheckTests(unittest.TestCase):
    def setUp(self):
        self.records = parsed(THREE)
        self.readme = generated(self.records)

    def edited(self, old, new):
        self.assertIn(old, self.readme)
        return self.readme.replace(old, new)

    def test_complete_index_passes_both_modes(self):
        for strict in (False, True):
            self.assertEqual(di.check(self.readme, self.records, strict=strict), ([], []))

    def test_unlisted_record_passes_default_and_is_named(self):
        extra = parsed({**THREE, "2026-10-29-man999-x.md": "MAN-999: x"})
        self.assertEqual(di.check(self.readme, extra, strict=False), ([], ["2026-10-29-man999-x.md"]))

    def test_unlisted_record_fails_strict(self):
        extra = parsed({**THREE, "2026-10-29-man999-x.md": "MAN-999: x"})
        problems, unlisted = di.check(self.readme, extra, strict=True)
        self.assertEqual(unlisted, ["2026-10-29-man999-x.md"])
        self.assertEqual(problems, ["does not list 2026-10-29-man999-x.md"])

    def test_row_for_missing_file_fails(self):
        gone = parsed({**THREE, "2026-10-29-man999-gone.md": "MAN-999: gone"})
        problems, _ = di.check(generated(gone), self.records, strict=False)
        self.assertEqual(len(problems), 1)
        self.assertIn("2026-10-29-man999-gone.md, which is not a record", problems[0])

    def test_wrong_date_or_ticket_cell_fails(self):
        for old, new in (("| 2026-10-27 | MAN-5 |", "| 2026-10-26 | MAN-5 |"),
                         ("| 2026-10-27 | MAN-5 |", "| 2026-10-27 | MAN-6 |"),
                         ("| 2026-10-27 | MAN-5 |", "| 2026-10-27 | — |")):
            with self.subTest(new=new):
                problems, _ = di.check(self.edited(old, new), self.records, strict=False)
                self.assertEqual(len(problems), 1, problems)
                self.assertIn("the record says 2026-10-27 | MAN-5", problems[0])

    def test_heading_ticket_change_passes_default_fails_strict(self):
        # A ticketless name's ticket comes from its heading, so a heading edit may change it.
        names = {**THREE, "2026-10-26-spike.md": "spike (MAN-53)"}
        readme = generated(parsed(names))
        edited = parsed({**THREE, "2026-10-26-spike.md": "spike (MAN-54)"})
        self.assertEqual(di.check(readme, edited, strict=False), ([], []))
        problems, _ = di.check(readme, edited, strict=True)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("2026-10-26-spike.md: row says 2026-10-26 | MAN-53", problems[0])

    def test_stale_title_passes_default_fails_strict(self):
        readme = self.edited("[MAN-5: a]", "[MAN-5: an older heading]")
        self.assertEqual(di.check(readme, self.records, strict=False), ([], []))
        problems, _ = di.check(readme, self.records, strict=True)
        self.assertEqual(problems, ["a row's title differs from its record's heading"])

    def test_out_of_order_rows_fail(self):
        a, b = self.records[0].row(), self.records[1].row()
        readme = self.edited(f"{a}\n{b}", f"{b}\n{a}")
        problems, _ = di.check(readme, self.records, strict=False)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("out of order", problems[0])

    def test_duplicate_row_fails(self):
        a = self.records[0].row()
        problems, _ = di.check(self.edited(a, f"{a}\n{a}"), self.records, strict=False)
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("listed twice", problems[0])

    def test_stray_line_in_block_fails(self):
        a = self.records[0].row()
        for stray in ("some prose", "", "| 2026-10-27 | MAN-5 | no link |"):
            with self.subTest(stray=stray):
                problems, _ = di.check(self.edited(a, f"{a}\n{stray}"), self.records, strict=False)
                self.assertEqual(len(problems), 1, problems)
                self.assertIn("not a `| date | ticket | [title](file) |` row", problems[0])

    def test_missing_table_header_is_malformed(self):
        with self.assertRaises(di.RecordError):
            di.check(self.edited(f"{di.HEADER}\n", ""), self.records, strict=False)


class CliTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = tmp.name
        write_records(self.dir, THREE)
        self.readme = os.path.join(self.dir, "README.md")
        with open(self.readme, "w", encoding="utf-8") as f:
            f.write(generated(parsed(THREE)))

    def test_exit_codes(self):
        for args in ((), ("--strict",)):
            proc = run_cli(*args, "--dir", self.dir)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertEqual(proc.stdout.strip(), "ok: docs/DECISIONS/README.md lists 3 of 3 records")
        write_records(self.dir, {"2026-10-29-man999-x.md": "MAN-999: x"})
        proc = run_cli("--dir", self.dir)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("lists 3 of 4 records; not listed yet", proc.stdout)
        self.assertIn("2026-10-29-man999-x.md", proc.stdout)
        proc = run_cli("--strict", "--dir", self.dir)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("does not list 2026-10-29-man999-x.md", proc.stderr)
        self.assertIn("python3 scripts/decisions-index.py --write", proc.stderr)
        with open(self.readme, encoding="utf-8") as f:
            text = f.read()
        with open(self.readme, "w", encoding="utf-8") as f:
            f.write(text.replace("| 2026-10-27 | MAN-5 |", "| 2026-10-20 | MAN-5 |"))
        proc = run_cli("--dir", self.dir)
        self.assertEqual(proc.returncode, 1)
        self.assertIn("docs/DECISIONS/README.md is generated: drop your edit to it", proc.stderr)
        with open(self.readme, "w", encoding="utf-8") as f:
            f.write("no markers\n")
        self.assertEqual(run_cli("--dir", self.dir).returncode, 2)
        write_records(self.dir, {"2026-10-29-Bad-Name.md": "x"})
        for args in ((), ("--strict",), ("--write",)):
            proc = run_cli(*args, "--dir", self.dir)
            self.assertEqual(proc.returncode, 2, args)
            self.assertIn("docs/DECISIONS/2026-10-29-Bad-Name.md: ", proc.stderr)

    def test_write_reports_row_count(self):
        write_records(self.dir, {"2026-10-29-man999-x.md": "MAN-999: x"})
        proc = run_cli("--write", "--dir", self.dir)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(proc.stdout.strip(), "wrote docs/DECISIONS/README.md: 4 rows")
        self.assertEqual(run_cli("--strict", "--dir", self.dir).returncode, 0)

    def test_help_names_the_modes_and_exit_codes(self):
        # A wide COLUMNS keeps argparse from wrapping at the hyphen in "lag-tolerant".
        out = subprocess.run([sys.executable, SCRIPT, "--help"], capture_output=True, text=True,
                             env={**os.environ, "COLUMNS": "1000"}).stdout
        for needle in ("--write", "--strict", "lag-tolerant", "Exit codes: 0 ok; 1", "2 a record or the README is malformed"):
            self.assertIn(needle, " ".join(out.split()))

    def test_default_dir_is_the_repo_records_from_any_cwd(self):
        proc = subprocess.run([sys.executable, SCRIPT], capture_output=True, text=True, cwd=self.dir)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(os.path.normpath(di.DEFAULT_DIR), DECISIONS)


class WiringTests(unittest.TestCase):
    def test_this_module_runs_in_the_required_test_job(self):
        job = wf.job_block(wf.read("ci-full.yml"), "test")
        found = gates.steps_running(job, TEST_COMMAND)
        self.assertEqual(len(found), 1)
        self.assertEqual(wf.field(found[0], "if"), "runner.os == 'Linux'")
        all_steps = wf.steps(job)
        here = next(i for i, s in enumerate(all_steps) if (wf.run_body(s) or "").strip() == TEST_COMMAND)
        pins = [i for i, s in enumerate(all_steps) if (wf.field(s, "uses") or "").startswith("actions/setup-python@")]
        self.assertTrue(pins and all(here < i for i in pins), "run before the setup-python pin")

    def test_claude_md_pre_push_list_names_this_module(self):
        with open(os.path.join(ROOT, "CLAUDE.md"), encoding="utf-8") as f:
            text = f.read()
        m = re.search(r"python3 -m unittest -v scripts\.tests\.test_\{([^}]*)\}", text)
        self.assertIsNotNone(m)
        self.assertIn("decisions_index", re.split(r"[,\s\\]+", m.group(1)))


class RealIndexTests(unittest.TestCase):
    """The committed index, checked the way a record PR's own CI sees it."""

    def setUp(self):
        with open(os.path.join(DECISIONS, "README.md"), encoding="utf-8") as f:
            self.readme = f.read()

    def test_every_record_parses(self):
        records = di.load_records(DECISIONS)
        self.assertEqual(len(records), len(glob.glob(os.path.join(DECISIONS, "*.md"))) - 1)

    def test_committed_index_passes_the_lag_tolerant_check(self):
        problems, _ = di.check(self.readme, di.load_records(DECISIONS), strict=False)
        self.assertEqual(problems, [], f"run python3 scripts/decisions-index.py --write: {problems}")

    def test_rust_name_pattern_matches_the_generator(self):
        with open(os.path.join(ROOT, "crates", "manta-cli", "tests", "docs_consistency.rs"), encoding="utf-8") as f:
            rust = f.read()
        m = re.search(r'const DECISION_RECORD_NAME: &str =\s*r"([^"]*)";', rust)
        self.assertIsNotNone(m, "docs_consistency.rs: no DECISION_RECORD_NAME literal")
        self.assertEqual(m.group(1), di.NAME.pattern)

    def test_preamble_says_record_prs_do_not_edit_the_table(self):
        preamble = " ".join(self.readme.split(di.BEGIN)[0].split())
        self.assertIn("scripts/decisions-index.py", preamble)
        self.assertIn("do not add a row", preamble)
        self.assertIn("python3 scripts/decisions-index.py --write", preamble)
        self.assertNotIn("decisions_index_links_every_record", self.readme)


class TwoRecordPrsScenarioTests(unittest.TestCase):
    """Gherkin Scenario 1: two open PRs each add a record; whichever merges first, the second
    still merges with no conflict, and the regenerated index lists both, newest first."""

    BASE = {"2026-10-20-man800-x.md": "MAN-800: x", "2026-10-21-man801-y.md": "MAN-801: y"}
    A = "2026-10-27-man901-a.md"
    B = "2026-10-28-man902-b.md"

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.env = wf.scrubbed_git_env(tmp.name)
        self.repo = os.path.join(tmp.name, "repo")
        self.dir = os.path.join(self.repo, "docs", "DECISIONS")
        write_records(self.dir, self.BASE)
        with open(os.path.join(self.dir, "README.md"), "w", encoding="utf-8") as f:
            f.write(generated(parsed(self.BASE)))
        self.git("init", "-q", "-b", "main")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "base")
        for branch, name, heading in (("a", self.A, "MAN-901: a"), ("b", self.B, "MAN-902: b")):
            self.git("checkout", "-q", "-b", branch, "main")
            write_records(self.dir, {name: heading})
            self.git("add", "-A")
            self.git("commit", "-q", "-m", f"add {name}")

    def git(self, *args):
        proc = subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
                               "-c", "commit.gpgsign=false", *args],
                              cwd=self.repo, env=self.env, capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, f"git {' '.join(args)}: {proc.stdout}{proc.stderr}")
        return proc

    def readme(self):
        with open(os.path.join(self.dir, "README.md"), encoding="utf-8") as f:
            return f.read()

    def test_each_record_branch_passes_its_own_ci(self):
        for branch, name in (("a", self.A), ("b", self.B)):
            with self.subTest(branch=branch):
                self.git("checkout", "-q", branch)
                self.assertEqual(di.check(self.readme(), di.load_records(self.dir), strict=False), ([], [name]))

    def test_record_only_branches_merge_cleanly_in_either_order(self):
        for first, second in (("a", "b"), ("b", "a")):
            with self.subTest(order=f"{first} then {second}"):
                self.git("checkout", "-q", "-B", "landed", "main")
                self.git("merge", "-q", "--no-ff", "--no-edit", first)
                self.git("merge", "-q", "--no-ff", "--no-edit", second)  # asserts exit 0: no conflict
                self.assertEqual(self.git("status", "--porcelain").stdout, "")
                self.assertEqual(di.check(self.readme(), di.load_records(self.dir), strict=False)[0], [])
                with contextlib.redirect_stdout(io.StringIO()):
                    self.assertEqual(di.main(["--write", "--dir", self.dir]), 0)
                readme = self.readme()
                self.assertEqual(di.check(readme, di.load_records(self.dir), strict=True), ([], []))
                self.assertLess(readme.index(f"]({self.B})"), readme.index(f"]({self.A})"))
                self.git("checkout", "-q", "--", ".")


def workflow_text():
    return wf.read(WORKFLOW)


def top_level(text, key):
    """The `key:` line at column 0 and everything nested under it."""
    lines = text.splitlines()
    i = lines.index(f"{key}:")
    return [lines[i]] + wf._block_lines(lines, i)


def keys(block):
    """Key names one level under block's header line."""
    indent = wf._indent(block[0]) + 2
    return [m.group(1) for line in wf.code_lines(block[1:])
            if (m := re.match(r"^ {%d}([A-Za-z_-]+):" % indent, line))]


def refresh_job():
    return wf.job_block(workflow_text(), "refresh")


def named_step(name):
    step = gates.step_by_name(refresh_job(), name)
    if step is None:
        raise AssertionError(f"{WORKFLOW}: no step {name!r}")
    return step


class RefreshWorkflowShapeTests(unittest.TestCase):
    def setUp(self):
        self.text = workflow_text()
        self.job = refresh_job()

    def test_triggers(self):
        on = top_level(self.text, "on")
        self.assertEqual(keys(on), ["push", "schedule", "workflow_dispatch"])
        push = wf.sub(on, "push")
        self.assertEqual(wf.field(push, "branches"), "[main]")
        self.assertEqual(wf.field(push, "paths"), "['docs/DECISIONS/**']")
        for forbidden in ("pull_request", "pull_request_target", "merge_group", "issue_comment", "workflow_run"):
            self.assertNotIn(forbidden, keys(on))

    def test_permissions(self):
        self.assertRegex(self.text, re.compile(r"^permissions: \{\}$", re.M))
        self.assertEqual(wf.job_ids(self.text), ["refresh"])
        perms = wf.sub(self.job, "permissions")
        self.assertEqual([line.strip() for line in perms[1:]], ["contents: read"])

    def test_runs_only_on_main_in_codex_review(self):
        self.assertEqual(wf.field(self.job, "if"), "github.ref == 'refs/heads/main'")
        self.assertEqual(wf.field(wf.sub(self.job, "environment"), "name"), "codex-review")

    def test_pat_is_only_in_the_refresh_step_env(self):
        secret_lines = [line.strip() for line in wf.code_lines(self.text) if "secrets." in line]
        self.assertEqual(secret_lines, ["GH_TOKEN: ${{ secrets.CODEX_REVIEW_PAT }}"])
        env = wf.sub(named_step("Open the refresh PR"), "env")
        self.assertEqual(wf.field(env, "GH_TOKEN"), "${{ secrets.CODEX_REVIEW_PAT }}")
        self.assertIsNone(wf.sub(named_step("Regenerate the index"), "env"))

    def test_run_bodies_hold_no_expressions(self):
        bodies = [wf.run_body(s) for s in wf.steps(self.job) if wf.run_body(s)]
        self.assertEqual(len(bodies), 2)
        for body in bodies:
            self.assertNotIn("${{", body)

    def test_checkout_pinned_like_ci_full_without_credentials(self):
        pin = re.compile(r"uses: (actions/checkout@[0-9a-f]{40})")
        ours = [s for s in wf.steps(self.job) if (wf.field(s, "uses") or "").startswith("actions/checkout@")]
        self.assertEqual(len(ours), 1)
        self.assertEqual(wf.field(wf.sub(ours[0], "with"), "persist-credentials"), "false")
        self.assertEqual(set(pin.findall(self.text)), set(pin.findall(wf.read("ci-full.yml"))))
        self.assertIsNone(wf.field(wf.sub(ours[0], "with"), "ref"), "check out the pushed main commit")

    def test_title_passes_the_pr_title_rule(self):
        title = wf.field(wf.sub(named_step("Open the refresh PR"), "env"), "TITLE").strip('"')
        self.assertEqual(title, TITLE)
        self.assertIsNone(title_rule.problem(title))

    def test_concurrency_is_one_serial_group(self):
        conc = top_level(self.text, "concurrency")
        self.assertEqual(wf.field(conc, "group"), "decisions-index")
        self.assertEqual(wf.field(conc, "cancel-in-progress"), "false")

    def test_main_alert_watches_it(self):
        self.assertRegex(self.text, re.compile(r"^name: Decisions Index$", re.M))
        lines = wf.read("main-alert.yml").splitlines()
        workflows = wf.sub(wf.sub(lines[lines.index("on:"):], "workflow_run"), "workflows")
        watched = [line.strip()[2:].strip() for line in wf.code_lines(workflows[1:]) if line.strip().startswith("- ")]
        self.assertIn("Decisions Index", watched)


# The contents PUT's --input file is copied to $PUT_BODY so a test can read the JSON it sent.
GH_STUB = """
case "$1 $2" in
  "pr list") printf '%s' "${PR_LIST_OUT:-}"; exit "${PR_LIST_RC:-0}" ;;
  "api -X")
    if [ "$3" = PATCH ]; then exit "${PATCH_RC:-0}"; fi
    if [ "$3" = PUT ]; then
      prev=""
      for a in "$@"; do if [ "$prev" = --input ]; then cp "$a" "$PUT_BODY"; fi; prev=$a; done
    fi ;;
esac
exit 0
"""


class RefreshStepBehaviourTests(unittest.TestCase):
    """The real "Open the refresh PR" body under bash -eo pipefail with a stub gh."""

    README_TEXT = "# Decision records\n\ngenerated\n"

    def run_refresh(self, **stub_env):
        step = named_step("Open the refresh PR")
        env = {name: wf.field(wf.sub(step, "env"), name).strip('"')
               for name in ("BRANCH", "README", "TITLE", "BODY")}
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.put_body = os.path.join(tmp.name, "put-body.json")
        # run_step's PATH is the stub dir, /usr/bin and /bin; the body also runs python3.
        path = f"{os.path.join(tmp.name, 'bin')}:{os.path.dirname(sys.executable)}:/usr/bin:/bin"
        env.update(GH_TOKEN="x", REPO=REPO, BASE_SHA=BASE_SHA, OLD_BLOB=OLD_BLOB, PATH=path,
                   PUT_BODY=self.put_body, **stub_env)
        os.makedirs(os.path.join(tmp.name, "docs", "DECISIONS"))
        with open(os.path.join(tmp.name, env["README"]), "w", encoding="utf-8") as f:
            f.write(self.README_TEXT)
        return gates.run_step(wf.run_body(step), env, GH_STUB, tmp=tmp.name)

    def assert_create_sequence(self, calls):
        ref = f"repos/{REPO}/git/refs/heads/{BRANCH}"
        patch = next(i for i, c in enumerate(calls) if gates.has_call([c], "PATCH", ref, f"sha={BASE_SHA}", "force=true"))
        put = next(i for i, c in enumerate(calls)
                   if gates.has_call([c], "PUT", f"repos/{REPO}/contents/docs/DECISIONS/README.md", "--input"))
        with open(self.put_body, encoding="utf-8") as f:
            sent = json.load(f)
        self.assertEqual(sent, {"message": TITLE, "branch": BRANCH, "sha": OLD_BLOB,
                                "content": base64.b64encode(self.README_TEXT.encode()).decode()})
        create = next(i for i, c in enumerate(calls)
                      if gates.has_call([c], "pr", "create", "--base", "main", "--head", BRANCH, "--title", TITLE))
        self.assertLess(patch, put)
        self.assertLess(put, create)
        self.assertEqual(create, len(calls) - 1)
        self.assertTrue(gates.arg_prefixed(calls[create], "Generated by .github/workflows/decisions-index.yml"))
        return patch

    def test_no_open_pr_points_the_branch_commits_and_opens(self):
        proc, _, calls = self.run_refresh()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertTrue(gates.has_call(calls[:1], "pr", "list", "--head", BRANCH, "--base", "main", "--state", "open"))
        self.assert_create_sequence(calls)
        self.assertFalse(gates.has_call(calls, "POST"))
        self.assertFalse(gates.has_call(calls, "close"))

    def test_missing_branch_is_created(self):
        proc, _, calls = self.run_refresh(PATCH_RC="1")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        patch = self.assert_create_sequence(calls)
        self.assertTrue(gates.has_call(calls[patch + 1:patch + 2], "POST", f"repos/{REPO}/git/refs",
                                       f"ref=refs/heads/{BRANCH}", f"sha={BASE_SHA}"))

    def test_open_mergeable_or_unknown_pr_is_left_alone(self):
        for state in ("MERGEABLE", "UNKNOWN"):
            with self.subTest(state=state):
                proc, _, calls = self.run_refresh(PR_LIST_OUT=f"12 {state}")
                self.assertEqual(proc.returncode, 0, proc.stderr)
                self.assertEqual(len(calls), 1, calls)
                self.assertEqual(calls[0][:2], ["pr", "list"])
                self.assertIn("::notice::Refresh PR #12 is open", proc.stdout)

    def test_conflicting_pr_is_closed_then_replaced(self):
        proc, _, calls = self.run_refresh(PR_LIST_OUT="12 CONFLICTING")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(calls[1][:3], ["pr", "close", "12"])
        self.assertTrue(gates.arg_prefixed(calls[1], "Conflicts with main"))
        self.assertGreater(self.assert_create_sequence(calls), 1)

    def test_a_failed_lookup_fails_the_step(self):
        proc, _, calls = self.run_refresh(PR_LIST_RC="1")
        self.assertNotEqual(proc.returncode, 0)
        self.assertEqual(len(calls), 1, calls)

    @unittest.skipUnless(shutil.which("jq"), "jq is not installed")
    def test_lookup_jq_reads_the_first_open_pr(self):
        body = wf.run_body(named_step("Open the refresh PR"))
        expr = re.search(r"--jq '([^']*)'", body).group(1)
        for given, want in (("[]", ""), ('[{"number":12,"mergeable":"CONFLICTING"},{"number":9,"mergeable":"MERGEABLE"}]',
                                         "12 CONFLICTING\n")):
            with self.subTest(given=given):
                out = subprocess.run(["jq", "-r", expr], input=given, capture_output=True, text=True, check=True)
                self.assertEqual(out.stdout, want)


class GenerateStepBehaviourTests(unittest.TestCase):
    """The real "Regenerate the index" body, run in a temp checkout with the real generator."""

    RECORDS = {"2026-10-20-man800-x.md": "MAN-800: x"}

    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = tmp.name
        self.env = wf.scrubbed_git_env(self.tmp)
        self.repo = os.path.join(self.tmp, "repo")
        self.dir = os.path.join(self.repo, "docs", "DECISIONS")
        os.makedirs(os.path.join(self.repo, "scripts"))
        shutil.copy(SCRIPT, os.path.join(self.repo, "scripts", "decisions-index.py"))
        write_records(self.dir, self.RECORDS)
        with open(os.path.join(self.dir, "README.md"), "w", encoding="utf-8") as f:
            f.write(generated(parsed(self.RECORDS)))
        self.git("init", "-q", "-b", "main")
        self.commit()

    def git(self, *args):
        proc = subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid",
                               "-c", "commit.gpgsign=false", *args],
                              cwd=self.repo, env=self.env, capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        return proc.stdout

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "c")

    def run_gen(self):
        out = os.path.join(self.tmp, "github_output")
        open(out, "w").close()
        script = os.path.join(self.tmp, "gen.sh")
        with open(script, "w", encoding="utf-8") as f:
            f.write(wf.run_body(named_step("Regenerate the index")))
        proc = subprocess.run(["bash", "--noprofile", "--norc", "-eo", "pipefail", script], cwd=self.repo,
                              env={**self.env, "GITHUB_OUTPUT": out}, capture_output=True, text=True)
        with open(out, encoding="utf-8") as f:
            return proc, f.read().splitlines()

    def test_current_index_sets_changed_false(self):
        proc, outputs = self.run_gen()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(outputs, ["changed=false"])
        self.assertIn("already lists every record", proc.stdout)

    def test_stale_index_sets_changed_true_and_old_blob(self):
        write_records(self.dir, {"2026-10-27-man901-a.md": "MAN-901: a"})
        self.commit()
        blob = self.git("rev-parse", "HEAD:docs/DECISIONS/README.md").strip()
        proc, outputs = self.run_gen()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(outputs, ["changed=true", f"old_blob={blob}"])
        self.assertEqual(self.git("status", "--porcelain"), " M docs/DECISIONS/README.md\n")
        self.assertEqual(di.check(self.readme(), di.load_records(self.dir), strict=True), ([], []))

    def test_any_other_changed_path_fails(self):
        write_records(self.dir, {"2026-10-27-man901-a.md": "MAN-901: a"})
        self.commit()
        with open(os.path.join(self.repo, "stray.txt"), "w") as f:
            f.write("x\n")
        proc, outputs = self.run_gen()
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("::error::scripts/decisions-index.py --write changed more than", proc.stdout)
        self.assertNotIn("changed=true", outputs)

    def readme(self):
        with open(os.path.join(self.dir, "README.md"), encoding="utf-8") as f:
            return f.read()


class DocsReferenceTests(unittest.TestCase):
    def test_workflow_and_preamble_name_the_man440_record_and_it_exists(self):
        records = [os.path.basename(p) for p in glob.glob(os.path.join(DECISIONS, "*-man440-*.md"))]
        self.assertEqual(len(records), 1, records)
        self.assertIn(f"docs/DECISIONS/{records[0]}", workflow_text())
        with open(os.path.join(DECISIONS, "README.md"), encoding="utf-8") as f:
            preamble = f.read().split(di.BEGIN)[0]
        self.assertEqual(MAN440_RECORD.findall(preamble), [records[0]])
        with open(SCRIPT, encoding="utf-8") as f:
            self.assertIn(f"docs/DECISIONS/{records[0]}", f.read())

    def test_record_supersedes_the_same_change_row_convention(self):
        path = glob.glob(os.path.join(DECISIONS, "*-man440-*.md"))[0]
        with open(path, encoding="utf-8") as f:
            text = f.read()
        self.assertIn("## Supersedes", text)
        superseded = text.split("## Supersedes")[1].split("\n## ")[0]
        for name in ("2026-10-16-man238-standing-brief.md", "2026-10-11-man149-docs-reference-and-index.md"):
            self.assertIn(name, superseded)
            self.assertTrue(os.path.isfile(os.path.join(DECISIONS, name)), name)


if __name__ == "__main__":
    unittest.main()
