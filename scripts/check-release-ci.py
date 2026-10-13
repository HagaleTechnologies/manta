#!/usr/bin/env python3
"""MAN-156: refuse to publish a release commit whose CI push run on main did not pass.

Windows legs are not required for merge (MAN-230, MAN-156), so a Windows-only failure can reach
main. release-publish.yml's validate-tag runs this before any platform build:

    check-release-ci.py OWNER/REPO COMMIT_SHA      (GH_TOKEN in the environment, read by `gh`)

It reads the newest ci-full.yml push run for the commit (a rerun's latest attempt counts) and
requires it completed and successful, with every leg in REQUIRED_JOBS present and green.
"""
import json
import re
import subprocess
import sys

WORKFLOW = "ci-full.yml"
REQUIRED_JOBS = (
    "test (ubuntu-latest)", "test (macos-latest)", "test (windows-latest)",
    "test-hpsdr (ubuntu-latest)", "test-hpsdr (macos-latest)", "test-hpsdr (windows-latest)",
)


def gh_json(path, **params):
    args = ["gh", "api", "-X", "GET", path] + [f"-f{k}={v}" for k, v in params.items()]
    return json.loads(subprocess.run(args, check=True, capture_output=True, text=True).stdout)


def verdict(sha, runs, jobs):
    """(ok, message). `runs` are the commit's push runs of WORKFLOW; `jobs` are the newest run's
    latest-attempt jobs."""
    if not runs:
        return False, (f"no {WORKFLOW} push run exists for {sha}. Tag a commit that CI tested on "
                       "main (GitHub keeps runs 90 days).")
    newest = max(runs, key=lambda r: r["run_number"])
    url = newest["html_url"]
    if newest["status"] != "completed":
        return False, f"CI run {url} for {sha} is still {newest['status']}; re-run this job once it finishes."
    by_name = {j["name"]: j["conclusion"] for j in jobs}
    bad = [f"{n}={by_name.get(n, 'missing')}" for n in REQUIRED_JOBS if by_name.get(n) != "success"]
    bad += [f"{n}={c}" for n, c in sorted(by_name.items())
            if n not in REQUIRED_JOBS and c not in ("success", "skipped")]
    if newest["conclusion"] != "success" or bad:
        return False, (f"CI run {url} for {sha} concluded {newest['conclusion']}: "
                       f"{', '.join(bad) or 'no failing job named'}. Tag a commit whose CI passed, "
                       "or re-run the failed jobs if they flaked, then re-run this job.")
    return True, f"OK: CI run {url} for {sha} passed, including {', '.join(REQUIRED_JOBS)}"


def main(argv):
    if len(argv) != 3 or not re.fullmatch(r"[0-9a-f]{40}", argv[2]):
        print("usage: check-release-ci.py OWNER/REPO <40-hex commit sha>", file=sys.stderr)
        return 2
    repo, sha = argv[1], argv[2]
    runs = gh_json(f"repos/{repo}/actions/workflows/{WORKFLOW}/runs",
                   head_sha=sha, event="push", per_page=100)["workflow_runs"]
    jobs = []
    if runs:
        newest = max(runs, key=lambda r: r["run_number"])
        jobs = gh_json(f"repos/{repo}/actions/runs/{newest['id']}/jobs",
                       filter="latest", per_page=100)["jobs"]
    ok, message = verdict(sha, runs, jobs)
    print(message if ok else f"::error title=Release commit CI::{message}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
