#!/usr/bin/env python3
"""MAN-245: list main-protection's required checks with the source each one may come from.

A ruleset matches a required check by name alone unless the entry carries an `integration_id`,
which pins it to one app. This reads a ruleset (or `rules/branches/main`) as JSON and prints one
line per required check: its name, the source the ruleset allows, and the decision recorded in
DECISION_RECORD -- pinned to one producer, or not pinned and why.

    gh api repos/HagaleTechnologies/manta/rulesets/18658768 | scripts/required-check-sources.py [RULESET_JSON]

Exit 0 when every required check matches its recorded decision; 1 when a recorded pin is not
applied, a check is pinned against the record, or a required check has no recorded decision; 2
when the input is not a ruleset with required checks.
"""
import json
import sys

DECISION_RECORD = "docs/DECISIONS/2026-10-16-man245-gate-verdict-integrity.md"
SOURCES = {15368: "GitHub Actions", 4696353: "hagale-agent"}
TEST_REASON = ("not pinned: hagale-agent (4696353) on trusted PR heads, GitHub Actions (15368) on "
               "Dependabot/fork/untrusted PR heads and merge groups")
CODEX_REASON = ("not pinned: hagale-agent (4696353) on non-Dependabot PR heads, GitHub Actions (15368) "
                "on Dependabot PR heads and merge groups")
# ("pin", integration_id, the owner step that applies it) or ("unpinned", the reason).
DECISIONS = {
    "test (ubuntu-latest)": ("unpinned", TEST_REASON),
    "test (macos-latest)": ("unpinned", TEST_REASON),
    "test (windows-latest)": ("unpinned", TEST_REASON),
    "Codex review clean (gate before expensive downstream jobs)": ("unpinned", CODEX_REASON),
    "Supply Chain": ("pin", 15368, "owner step O2"),
    "PR title": ("pin", 15368, "owner step O2's command, run for PR title when MAN-156 O4 requires it"),
}
USAGE = ("usage: gh api repos/HagaleTechnologies/manta/rulesets/18658768 | "
         "scripts/required-check-sources.py [RULESET_JSON]")


def source(integration_id):
    if integration_id is None:
        return "any source"
    return f"{SOURCES.get(integration_id, 'app')} ({integration_id})"


def verdict(context, integration_id):
    """(ok, text) for one required check against its recorded decision."""
    decision = DECISIONS.get(context)
    if decision is None:
        return False, f"NO RECORDED DECISION: add one to DECISIONS and {DECISION_RECORD}"
    if decision[0] == "pin":
        _, wanted, step = decision
        if integration_id == wanted:
            return True, "pinned as recorded"
        if integration_id is None:
            return False, f"PIN NOT APPLIED: record says {source(wanted)} -- {step}"
        return False, f"PINNED TO {source(integration_id)} BUT recorded {source(wanted)}"
    if integration_id is None:
        return True, decision[1]
    return False, f"PINNED TO {source(integration_id)} BUT recorded {decision[1]}"


def required_checks(doc):
    """[(context, integration_id or None)] from a ruleset object or a list of rules; None when the
    input holds no required_status_checks rule."""
    rules = doc.get("rules") if isinstance(doc, dict) else doc
    if not isinstance(rules, list):
        return None
    found, seen = [], False
    for rule in rules:
        if not isinstance(rule, dict) or rule.get("type") != "required_status_checks":
            continue
        seen = True
        for entry in (rule.get("parameters") or {}).get("required_status_checks") or []:
            found.append((entry.get("context", ""), entry.get("integration_id")))
    return found if seen else None


def main(argv, stdin=sys.stdin, stdout=sys.stdout, stderr=sys.stderr):
    if len(argv) > 2:
        print(USAGE, file=stderr)
        return 2
    try:
        if len(argv) == 2:
            with open(argv[1], encoding="utf-8") as f:
                doc = json.load(f)
        else:
            doc = json.load(stdin)
    except (OSError, ValueError) as e:
        print(f"required-check-sources: cannot read the ruleset JSON: {e}\n{USAGE}", file=stderr)
        return 2
    checks = required_checks(doc)
    if checks is None:
        print(f"required-check-sources: no required_status_checks rule in the input\n{USAGE}", file=stderr)
        return 2
    rows = [(context, source(iid), *verdict(context, iid)) for context, iid in checks]
    width = max((len(r[0]) for r in rows), default=0)
    src_width = max((len(r[1]) for r in rows), default=0)
    for context, src, _, text in rows:
        print(f"{context:<{width}}   {src:<{src_width}}   {text}", file=stdout)
    print(f"decisions: {DECISION_RECORD}", file=stdout)
    return 0 if all(r[2] for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
