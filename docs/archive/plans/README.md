# Archive: AI agent plans and specs (July to September 2026)

This directory is a historical record, not design documentation. Nothing
here is normative, and nothing here is kept current.

The files are the working plans and design specs that AI coding agents
wrote and followed while building manta's M0 to M3 sub-projects and the
decode-core v2 work, from July to September 2026. They came from the
`superpowers` agent skill set: every file in `plans/` opens with a
"For agentic workers" line naming one of its skills. They include agent
task steps and command text such as `git commit` and `gh pr create`.

- `plans/`: 13 step-by-step implementation plans.
- `specs/`: 12 design specs, the counterpart of those plans.

They lived in `docs/superpowers/` until MAN-480 moved them here, per
decision D12 of the
[2026-09-06 broad review](../../DECISIONS/2026-09-06-broad-review-decisions.md).
They are left as written, so paths inside them still say
`docs/superpowers/` and may no longer resolve. Some files elsewhere in
the repository still cite the old paths too, until a later MAN-150
change updates them. Read any `docs/superpowers/` path as
`docs/archive/plans/`: the layout below that prefix is unchanged. Git
history has every earlier version.

For how manta works and why, read these instead:

- [SPEC-decode-core.md](../../SPEC-decode-core.md) and
  [SPEC-decode-core-v2.md](../../SPEC-decode-core-v2.md): the decode
  algorithm and its golden vectors.
- [ARCHITECTURE.md](../../../ARCHITECTURE.md): the crates and how data
  flows between them.
- [DECISIONS/README.md](../../DECISIONS/README.md): one record per ticket,
  with the measurements and gates behind each choice.
