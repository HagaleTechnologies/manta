# MAN-217: native merge-queue admission ignores ruleset bypass_actors

`manta-review-gate`'s `required_approving_review_count: 1` (added alongside the native merge-queue
cutover, PR #185) was meant to be satisfied for `thagale`/`catalyst-cloud-connector[bot]` via the
ruleset's own `bypass_actors` mechanism, mirroring `.mergify.yml`'s retired trusted-author allowlist.

Landing PR #186 surfaced that this doesn't work: a fully green PR with auto-merge armed as `thagale`
(a registered bypass actor) sat with `mergeable_state: blocked` / `reviewDecision: REVIEW_REQUIRED`
for 6+ hours, and no `merge_group` run was ever created -- it never actually entered the native
queue. Only a manual `gh pr merge --admin --squash` worked.

**Root cause:** `bypass_actors` and `--admin` are different mechanisms. `bypass_actors` exempts an
actor from a rule during an *explicit* admin-style merge action, not automatic auto-merge/queue
admission -- auto-merge is deliberately designed to never cut corners; it only enqueues once every
rule is satisfied for real, with no bypass consideration. `gh pr merge --help` names `--admin`, not
`bypass_actors`, as the way to bypass a queue-governed branch. Widdershins never hit this because its
equivalent ruleset has `required_approving_review_count: 0` -- there was nothing to bypass.

**Fix:** reverted `required_approving_review_count` to `0` on `manta-review-gate`. `bypass_actors`
stays configured (still useful defense-in-depth for a manual/admin merge path), and the ruleset's
other fields (`required_review_thread_resolution: true`, etc.) are unaffected -- they don't depend on
the review-count and keep working. The actual admission gate remains what it always was:
`auto-merge-trigger.yml`/`dependabot-auto-merge.yml`'s own trusted-author `if:` checks, matching
`.mergify.yml`'s original design intent, just reimplemented natively.

This PR is itself the live verification: a trusted-author (thagale) PR, armed by
`auto-merge-trigger.yml`, expected to actually enter the merge queue and merge automatically this
time -- unlike PR #186, which needed a manual admin override.

See [MAN-217](https://linear.app/hagaletechnologies/issue/MAN-217).
