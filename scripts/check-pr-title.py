#!/usr/bin/env python3
"""MAN-156: squash merge makes a PR's title the commit subject on main, so the title must be a
Conventional Commits header: `<type>(<scope>): <subject>`, `<scope>` optional, `!` before the
colon for a breaking change. GitHub's own `Revert "<title>"` is accepted too.

Usage: check-pr-title.py TITLE   (exit 0 OK, 1 not conventional, 2 usage)
"""
import re
import sys

TYPES = ("build", "chore", "ci", "docs", "feat", "fix", "perf", "refactor", "revert", "style", "test")
HEADER = re.compile(r"^(?P<type>[a-z]+)(?:\((?P<scope>[^()\r\n]+)\))?!?: \S")
GITHUB_REVERT = re.compile(r'^Revert ".+"$')
EXAMPLE = "fix(spot): reject a callsign with no digit"


def problem(title):
    """None when `title` is acceptable, else what is wrong with it."""
    if GITHUB_REVERT.match(title):
        return None
    m = HEADER.match(title)
    if not m:
        return 'it does not start "<type>: " or "<type>(<scope>): "'
    if m.group("type") not in TYPES:
        return f'"{m.group("type")}" is not one of: {", ".join(TYPES)}'
    return None


def main(argv):
    if len(argv) != 2:
        print("usage: check-pr-title.py TITLE", file=sys.stderr)
        return 2
    why = problem(argv[1])
    if why is None:
        print(f"OK: {argv[1]}")
        return 0
    print(f"::error title=PR title is not a Conventional Commits header::{argv[1]!r}: {why}. "
          "Squash merge makes this title the commit subject on main. Use "
          f'"<type>(<scope>): <subject>", for example "{EXAMPLE}", then edit the title; '
          "this check re-runs on the edit.")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
