# MAN-65: release pipeline deferred review findings (PR #78)

MAN-65 tickets four review findings deferred from PR #78's later
(chatgpt-codex-connector) round, per
`docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`. All four were
re-verified against `main` at `5b9e747` before this work started: findings
1 and 3 had no mitigation of any kind; findings 2 and 4 had a
documentation-only mitigation already committed, matching each finding's
own stated interim disposition.

## Finding 1 — SemVer build-metadata tags break the Docker tag grammar

> The workflow accepts every `v*` Git tag, but copies the suffix directly
> into an OCI tag. A valid release tag such as `v1.2.3+linux` (SemVer build
> metadata is also valid in a Git ref) produces `ghcr.io/...:1.2.3+linux`;
> `+` is not permitted by Docker's tag grammar, so Buildx rejects the image
> and the dependent GitHub release is skipped.

**Options considered:**

1. Restrict accepted release refs (reject the tag outright).
2. Translate/truncate build metadata into a legal tag (e.g. `1.2.3-linux`
   or drop the suffix to `1.2.3`).
3. Validate late, inside `docker-publish`, right before the Buildx call.

**Decision: (1), gated early in its own job (`validate-tag`).** Manta
publishes one version across all five release targets from one tag — a
per-platform build-metadata suffix has no meaning in this project's release
model, so (2) would invent a second naming convention nobody intends to
use, and truncating to `1.2.3` would silently collide with a real `v1.2.3`
tag pushed separately. Rejecting loudly is correct. (3) was rejected
because it still burns five platform builds (~30 minutes of runner time)
before failing; a dedicated `validate-tag` job fails in seconds, before
`build` even starts, and is also now the single place the version/image
strings are derived (both `build` and `docker-publish` consume its
outputs), which is why the ~35-line inline "Determine version, image, and
tag list" step could be deleted from `docker-publish` entirely.

The check is two-layered: `scripts/release-version.sh validate` enforces
SemVer 2.0.0 minus the build-metadata production (the *policy*), then
`assert-oci-tag` enforces the actual OCI grammar
(`[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`, the *guarantee*) — this second layer
also covers the `workflow_dispatch` path's `dispatch-<run_id>` tag, which
never goes through SemVer validation at all. The tag-push trigger glob was
also tightened from `v*` to `v[0-9]*` in both workflows: this doesn't (and
can't) express the full grammar, but it stops an unrelated tag like
`vendor-freeze` from invoking either release workflow at all; `validate-tag`
is still what gives a near-miss release tag (`v1.2.3+linux`) its explicit,
readable rejection.

## Finding 2 — Windows archive isn't self-contained (needs VC++ Redistributable)

> On a clean Windows installation without the Visual C++ 2015-2022
> Redistributable, the archived executable cannot start because
> `x86_64-pc-windows-msvc` uses the dynamic MSVC runtime by default, while
> this ZIP contains only `manta.exe`, the README, and licenses.

**Options considered:**

1. Static CRT (`-C target-feature=+crt-static`).
2. Bundle the Visual C++ Redistributable installer in the ZIP.
3. Document the caveat only (the status quo at the time of research).

**Decision: (1).** Bundling (2) adds a redistribution/licensing question
and an install step to a path whose entire selling point is
unpack-and-run. Static linking is safe for this dependency graph: `cargo
build -p manta-cli` compiles no C code for this target (the only
`cc`-using crate in `Cargo.lock`, `alloca`, is reachable only through
`criterion`, a dev-dependency never built by a release build), and Windows
platform access goes entirely through `windows-sys` import libraries
against system DLLs — there is no `ring`/`openssl`/`aws-lc` in the
lockfile to conflict with a static CRT.

**Deviation from the original plan, and why:** the plan called for setting
this flag once, repo-wide, via `.cargo/config.toml` — reasoning that two
workflow files build this same target, so a flag set in only one of them
(or duplicated and allowed to drift) is worse than a single source of
truth, and that a contributor's local Windows build should match CI by
default. That remains the right design in the abstract, but the automated
environment this ticket was implemented in refuses to write **any** file
under `.cargo/` at all (a blanket guardrail against unattended changes to
cargo's own config/credentials surface, not something this run can get an
exception to). The flag is therefore set per-matrix-entry instead:
`rustflags: '-C target-feature=+crt-static'` on the
`x86_64-pc-windows-msvc` entry in both `release.yml`'s and
`release-publish.yml`'s build matrices, consumed by the existing
`Build (native)` step via `env: RUSTFLAGS: ${{ matrix.rustflags }}`. The
two copies carry an explicit "must not drift" cross-reference comment to
each other. **Accepted trade-off:** a contributor building manta for
Windows locally does not get the static CRT automatically the way a
`.cargo/config.toml` would have given them — they would need to set
`RUSTFLAGS` themselves. This doesn't affect what's actually shipped (CI is
what produces every released artifact), which is the finding's real
concern. A future PR authored by someone with the access this run lacked
could still move this into `.cargo/config.toml` if the duplication ever
becomes a real drift problem; nothing here forecloses that.

Either way, the flag alone is not self-verifying — a `RUSTFLAGS`
environment variable set anywhere in the job (or, in the original design,
in a contributor's shell) can silently override it, since Cargo replaces
rather than merges `RUSTFLAGS` sources. Both workflows therefore also gained
an "Assert the Windows binary is statically CRT-linked" step, run
immediately after the Windows build: it reads the produced `manta.exe`'s
raw bytes and asserts `VCRUNTIME140`/`MSVCP140`/`api-ms-win-crt-*` do NOT
appear as import names, with a self-validity check (`KERNEL32.dll` MUST be
found) so a scan that silently looked at the wrong bytes fails loudly
rather than reporting a false pass.

**Not provable by CI, by nature of the property:** whether `manta.exe`
actually *starts* on a Windows machine that has never had the
Redistributable installed. GitHub's `windows-latest` runner image already
has the Redistributable present, so a successful run there proves nothing
either way. This is recorded as a manual verification step
(`docs/RUNBOOKS/release.md`), not an automated one.

## Finding 3 — Overlapping tag builds can race on the shared `:latest` tag

> Each release tag gets a different concurrency group because this
> expression includes `github.ref`, but every tag workflow later writes
> the same Docker `:latest` tag. If two tags are pushed close together,
> their builds can overlap and an older release that finishes last
> overwrites `latest` with the older image.

**Options considered:**

1. Make the whole `release-publish` workflow's concurrency group
   repo-wide (drop `github.ref` from the top-level group).
2. Publish `:latest` only after verifying, in a dedicated job with its own
   repo-wide concurrency group, that the current tag is still the newest
   release.

**Decision: (2).** GitHub Actions keeps at most one *pending* run/job per
concurrency group and cancels the previous pending one on a new arrival.
Making the whole workflow's group repo-wide (1) would mean a middle tag's
entire release — every platform build, the GitHub Release, everything —
could be silently cancelled by a later tag arriving before it finishes,
not just its `:latest` write. Scoping a repo-wide group to a job
(`publish-latest`) that does nothing BUT write `:latest` makes that same
cancellation semantics correct instead of harmful: the only thing a
cancellation can ever drop is a `:latest` write from a run that was about
to decline it anyway.

`publish-latest` re-fetches tags and re-runs
`scripts/release-version.sh is-newest-stable` immediately before writing
`:latest` — deliberately *after* the multi-arch build, not at job start
(`actions/checkout` defaults to `fetch-depth: 1` and fetches no tags at
all, so an early check would answer a stale question). It then re-points
`:latest` with `docker buildx imagetools create`, a manifest-only copy of
an already-pushed image rather than a rebuild, which shrinks the residual
race window from "the whole multi-arch build" to "the gap between the
recency check and one registry call." That residual window is accepted,
not closed: two tags pushed within that few-second gap could still resolve
out of order. Closing it fully would need an external lock or a
reconciler workflow, disproportionate for a project where one maintainer
pushes one release tag at a time — and recoverable in one command
(`docs/RUNBOOKS/release.md`'s recovery section).

`release` (the GitHub Release job) deliberately does **not** depend on
`publish-latest` — a declined or concurrency-cancelled `:latest` write must
never block a GitHub Release from being created.

**An unnamed second defect, fixed here in the same three lines:**
`is-newest-stable` treats any pre-release (`v1.3.0-rc.1`) as never eligible
for `:latest`, regardless of its numeric ordering against stable tags —
the original code had no such distinction, so a pre-release tag pushed
after the latest stable release would previously have overwritten
`:latest` with a release candidate. README's install command must always
hand users a release, not an RC.

**A deliberate asymmetry, pinned by test:** a tag with a numerically higher
version prefix but an invalid suffix (e.g. `v1.2.4+meta`, which
`validate-tag` would itself reject and which therefore never gets
published) still counts as "something newer might exist" and blocks an
older *valid* tag from claiming to be newest — unlike a pre-release, which
never blocks. The reasoning: a malformed tag might still get corrected and
re-pushed as the real next release, so it's treated conservatively; a
pre-release is intentionally non-final and blocking on it would freeze
`:latest` for as long as an RC cycle runs.

## Finding 4 — New GHCR package defaults to private

> On the first publication of this new GHCR package, GitHub Container
> Registry creates it with private visibility by default; this workflow
> authenticates and pushes the image but neither changes that visibility
> nor documents the required one-time package setting.

**Decision: not automated, but detected and documented.** GHCR container
package visibility has no supported REST endpoint reachable from a
workflow's `GITHUB_TOKEN` — the only documented paths are the web UI and
repo-linked inherited access. Automating this would require provisioning
and storing a long-lived personal access token with `admin:packages` in
this repo purely to flip one switch once, on one package, one time — a
strictly worse security posture than a single manual click. This is the
same "flag it to a human, don't silently work around it" disposition this
repo already applies to MAN-66
(`.github/workflows/release-publish.yml`'s own header comment, the
protected-Environment gap for `packages: write`). **This closes the
ticket's own open "or an explicit decision to automate it via the GitHub
API in a future PR" option — it is not deferred, it is decided against.**

What this PR fixes is the finding's other half: "neither changes that
visibility **nor documents the required one-time package setting**." The
`publish-latest` job's final step performs an anonymous-pull probe against
the just-published version tag on every release and writes the result to
the run's own `$GITHUB_STEP_SUMMARY` — either an "OK" line, or a warning
block naming the exact click-path (package settings → Danger Zone → Change
visibility → Public) and pointing at `docs/RUNBOOKS/release.md`. The step
is written to never fail the job: the release itself is fine either way,
and the fix is an out-of-band human action, so a hard failure here would
misreport a successful release as broken. `README.md`'s parenthetical
caveat now points at the runbook instead of restating the mechanics
inline, and the runbook is the durable record — no longer knowledge held
only in a README parenthetical and a Linear ticket.

## What this does not fix

- `:latest` for two tags pushed within the same few-second window (Finding
  3's accepted residual risk, with a one-command recovery documented in
  `docs/RUNBOOKS/release.md`).
- Whether `manta.exe` actually starts on a Windows machine that has never
  had the VC++ Redistributable installed — CI's own Windows runner already
  has it, so this can only be verified manually (Finding 2).
- GHCR package visibility itself — a deliberate, permanent human step, not
  a gap awaiting a future fix (Finding 4).
- MAN-66 (the protected-Environment gap for `packages: write` in
  `release-publish.yml`) — a separate ticket, flagged to a human directly,
  out of scope here.

## References

- Ticket: MAN-65. PR #78
  (`https://github.com/HagaleTechnologies/manta/pull/78`),
  chatgpt-codex-connector review round, deferred under
  `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.
- Sibling disposition precedent: MAN-66
  (`.github/workflows/release-publish.yml:26-42`).
- Runbook: `docs/RUNBOOKS/release.md`.
- Code: `scripts/release-version.sh`,
  `scripts/tests/release-version.test.sh`,
  `.github/workflows/release.yml`, `.github/workflows/release-publish.yml`,
  `.github/workflows/ci.yml` (`test` job), `README.md`.
