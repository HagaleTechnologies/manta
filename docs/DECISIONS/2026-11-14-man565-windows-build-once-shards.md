# MAN-565: build Windows test executables once and run eight prebuilt shards

Date: 2026-11-14. Ticket: MAN-565.
Status: implementation prepared; native hosted smoke, full-graph transfer and
under-35-minute main-run timing acceptance remain pending. Local Linux results
cannot establish Windows portability or hosted latency.

## Decision and scope

Windows uses MAN-518's build-once bundle model on GitHub-hosted
`windows-latest`. MAN-517's old per-shard-build branch is prior art only.
The graph is:

- `windows-support` runs independently of the build. It checks formatting,
  clippy, doctests and release archive scripts, then uploads its support record.
- `windows-build` invokes the ordinary workspace test-executable build once,
  lists all tests and ignored tests, assigns eight groups using the existing
  shared weights, and uploads one compressed tar slice per group.
- `windows-shards` needs only the build and has eight indices, 0 through 7.
  Each downloads its own slice and runs complete executables with normal
  libtest threading. It installs no toolchain and restores no compiler cache.
- `windows-aggregate`, running briefly on Ubuntu, retains the exact check name
  `test (windows-latest)`. It succeeds only when support, build and shards all
  succeeded. Failed, cancelled, skipped or missing dependencies fail closed.

The Windows relay now needs this aggregate. MAN-516's independent Unix relay,
MAN-371's check-run naming and the release workflow's static-CRT assertion stay
intact. Required-check ruleset changes remain the owner's work. No self-hosted
runners or additional macOS runners are introduced.

Support compilation for clippy and doctests is allowed. Only the build job
produces the ordinary Cargo workspace test-executable inventory. The initial
placement uses the same weights as Unix. A separate Windows weight column
needs measured imbalance before it is justified.

## Native tripwire and child environments

During bundle creation on Windows, the pinned `rustc` compiles the std-only
`scripts/ci-shard-tripwire.rs` once to `target/ci-shard-tripwire.exe`.
Each Windows version-1 slice includes that helper and an additive
`tripwireExecutable` workspace-relative path. The helper is outside the Cargo
test inventory. Bundle execution validates that it is a regular extracted file
inside the target directory and rejects absent, absolute or traversing paths.
Unix version-1 plans need no new field.

Before listing or running a slice, the shard copies the helper to a temporary
directory as `cargo.exe`, `rustc.exe` and `rustup.exe`. It never compiles the
helper on the shard. Native Rust `Command` calls to the tool names, explicit
`.exe` forms and the absolute `CARGO` path exit 97 with the existing
`MAN-518 tripwire: a shard invoked <tool>` diagnostic. Windows batch wrappers
would not provide that Rust process-lookup guarantee.

Windows DLL directories prepend the inherited `PATH`, and the tripwire
precedes those directories. Package Cargo variables remain available.
Linux keeps `LD_LIBRARY_PATH`, macOS keeps `DYLD_FALLBACK_LIBRARY_PATH`, and
both keep their extensionless shell tripwires. Source, JSON and subprocess
text uses explicit UTF-8 so the runner's locale cannot corrupt listing names
or socket-test selection.

The bundle retains MAN-518's absolute-checkout-root assumption. Cargo embeds
paths to package manifests, CLI binaries and target temporary directories.
Hosted build and shard jobs of the same OS must use the same checkout root.
A different root, source commit, shard index or count fails before execution;
listing drift fails before ordinary tests run. Archive member checks continue
to reject links, absolute names and paths outside the target directory.
PDB files are not packaged. Backtrace detail may be reduced; add PDB packaging
only if an observed debugging failure requires it.

## Failures, repeats and evidence

A failing executable records its nonzero exit and the shard continues through
later ordinary executables, then exits nonzero. Ordinary coverage stays exactly
once across the eight slices. MAN-523's changed socket tests run ten additional
times in their assigned slice, using the existing event-derived repeat base.
A repeat failure fails the shard; no retry turns failure green. Records keep
`repeatPlan` and `repeatResults` separate from ordinary `results`.

`windows-build` reuses the former serial job's rust-cache key through
`shared-key: test`; support gets its own job key. Caches save only on a push to
main. Build evidence is `build-windows-latest`; shard evidence is
`shard-windows-latest-<i>` and support evidence is `support-windows`.
Evidence upload runs even after failure. Slices are
`test-bundle-windows-latest-<i>` with compression level 0 and one-day retention.
Missing slice files fail upload or download. Use **Re-run all jobs** if a
slice expired or a rerun cannot download an earlier attempt's artifact.

## Native smoke and rollout validation

`.github/workflows/ci-windows-bundle-smoke.yml` runs the portable tiny Cargo
fixture on `pull_request` opened, reopened, synchronize and ready-for-review
events, including trusted authors. It has only `contents: read`, suppresses
checkout credentials, and has no secrets, environment, cache writes or App token.
Its distinct check name is `Windows bundle smoke`; it does not create another
`test (windows-latest)` check. The suite also runs in both Unix support jobs.
The fixture checks bundle coverage, ignored sets, CLI availability, a failure
followed by a passing executable, UTF-8 repeats and native Rust process lookup.

The smoke check proves fixture portability only when it actually passes on
Windows. A dispatched full run still reads main's workflow during review. The
merge-group run is the first normal execution of the new full graph. Inspect
that run for one Windows build, eight slice downloads, compiler-free shards and
the aggregate. Windows is not currently required, so queue success alone is
not evidence that this graph passed.

For the first successful push-to-main run using this graph, collect the run's
`created_at` and all paginated latest-attempt Jobs API rows. Compute elapsed as
`max(completed_at)` over `windows-support`, `windows-build` and all eight
`windows-shards` jobs, minus run `created_at`. Include queue delay and the
support tail. Require elapsed strictly below 2,100 seconds. Report aggregate
and relay completion separately; exclude Unix-only Soapy jobs. The 50-minute
shard timeout accommodates expensive socket repeats and does not change the
35-minute acceptance threshold.

Example evidence collection, with `RUN` set to the observed run id:

```sh
gh api "repos/HagaleTechnologies/manta/actions/runs/$RUN" > windows-run.json
gh api --paginate "repos/HagaleTechnologies/manta/actions/runs/$RUN/jobs?per_page=100" > windows-jobs.json
gh run download "$RUN" --pattern 'build-windows-latest' --dir windows-evidence/build
gh run download "$RUN" --pattern 'support-windows' --dir windows-evidence/support
gh run download "$RUN" --pattern 'shard-windows-latest-*' --dir windows-evidence/shards
```

Build `windows-audit-input.json` with `shards` holding all eight records,
`support` holding the support record, `workflowStarted` and `completed` as Unix
epoch timestamps, `build` holding the build record, and `reference` holding an
independent same-source unsharded inventory. Obtain that reference through a
separately authorized hosted validation run with the same toolchain and default
features, using `--inventory-only`. Then run:

```sh
python scripts/ci-shards.py --audit windows-audit-input.json --output windows-audit.json
```

Require unique, complete ordinary results, matching ignored sets, no nonzero
exits, one tested source identity and zero shard `buildSeconds`. An audit against
the build's own inventory only checks execution coverage and must be labelled
that way until the independent reference exists. Repeat results are additional
runs, not ordinary duplicate coverage.

Record actual inventory counts, compile/pack/extract/transfer durations, each
shard duration, support duration and elapsed in this record after hosted runs.
If elapsed is at least 2,100 seconds, identify queueing, support, compilation,
transfer or placement as the demonstrated bottleneck and repair it before
claiming acceptance. Do not lower the threshold, remove repeats or skip binaries.
The research estimate is not a hosted measurement.

Rollback is a normal merge-queue revert of the graph and related script changes
together. Legacy script mode stays tested while dispatched workflows still use
the previous graph.
