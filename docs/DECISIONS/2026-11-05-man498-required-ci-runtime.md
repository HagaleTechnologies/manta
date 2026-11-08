# MAN-498: shard required Unix workspace tests by executable

Required Ubuntu and macOS CI now use eight whole-binary shards and a concurrent
support job per OS. The OS aggregate retains `test (ubuntu-latest)` or
`test (macos-latest)` and requires both dependencies to succeed. Windows keeps
`test (windows-latest)` and its serial Cargo command. The dispatch relay waits
for both Unix aggregates and Windows, then relays each exact job's conclusion.
A failed, cancelled, skipped or missing dependency cannot pass the aggregate.
Native jobs retain the trusted-author routing, reviewed checkout, inline merge
with main, suppressed checkout credentials and main-push-only cache writes.

<<<<<<< HEAD
**Superseded in part (2026-11-08):** MAN-517 shards Windows the same way;
Windows no longer runs `cargo test --workspace --locked` serially. See
[its record](2026-11-08-man517-shard-the-windows-test-leg.md).
=======
**Superseded in part by MAN-516 (2026-11-08):** "The dispatch relay waits for
both Unix aggregates and Windows" no longer holds. The relay is two jobs, so
the Unix check-runs land when their aggregates finish; see
[its record](2026-11-08-man516-relay-unix-check-runs-before-windows.md).
>>>>>>> bd701c6ccf554bd0e7ed418b9787e4f22ce9c3ac

This implements scheduling, not a measured 20-minute hosted outcome. The merge
queue timeout remains 90 minutes until the rollout below passes. No decoder
scene, test assertion, repetition count, ignore annotation or timeout changed.
No coverage moved to a scheduled or optional job.

## Coverage and execution

`scripts/ci-shards.py` builds the feature-unified default workspace with
`cargo test --workspace --locked --no-run --message-format=json`. Compiler
artifacts identify actual executable targets, including library and binary unit
tests and integration tests. Duplicate messages are collapsed; conflicting
artifacts, unknown executable kinds and malformed JSON fail closed. Targets
excluded by Cargo's default features stay excluded. New targets enter the
schedule automatically with a 30-second estimate. Timing estimates never filter
coverage. `golden_v8_v8w` has shard zero to itself; deterministic longest-first
assignment spreads all other targets across seven workers. Each worker invokes
one binary at a time with normal libtest threading, preserving process-local
locks. Targets run from their package directories with Cargo manifest variables
and dynamic-library paths. Compiled `CARGO_BIN_EXE_manta` paths remain intact.

Every shard inventories test and ignored names using libtest listing before
execution. Unsupported listing fails the audit rather than reporting no tests.
Artifacts contain complete discovery, selected targets, child commands, package
working directories, exit codes, build and binary durations and runner identity.
Each OS support uploads its conclusion and explicit doctest outcome. Uploads
run on failure, and never suppress a failed test.

All old required commands retain a required home:

- Default Unix unit and integration tests run in shards. Doctests run once per
  OS with `cargo test --workspace --doc --locked` in support.
- Formatting, workspace clippy, Linux rustdoc, Unix FT8/FT4 clippy and tests,
  release shell suites and all Python suites retain their platform conditions
  in support. The new shard regression suite also runs in both Unix supports.
- Windows keeps the original command sequence and platform guards. Its package
  release script suite still runs. Soapy is unchanged.
- Catalyst's serial workspace gate remains the local reference command.

The real Cargo fixture exercises library, CLI, integration, required-feature
exclusion, ignored and doc tests, CLI execution and package-relative files.
Regression tests exercise failed children/builds, scheduling, evidence omissions,
ignored-set drift, duplicate targets, aggregate cancellation and missing results,
and independent relay conclusions with stub Jobs and Checks API calls.

## Reproducing coverage and timing evidence

Run the serial reference and shard discovery on the same build, OS and features:

```sh
cargo test --workspace --locked -- --list > cargo-list.txt
python3 scripts/ci-shards.py --inventory-only --output inventory.json
for i in 0 1 2 3 4 5 6 7; do
  python3 scripts/ci-shards.py --shard "$i" --output "shard-$i.json"
done
cargo test --workspace --doc --locked
```

The loop is serial for a local correctness comparison. CI runs each shard on
its own host. Compare the multiset of `: test` and `: benchmark` listing names
from Cargo against the full inventory. Compare ignored names per target with
`--list --ignored`; do not count an ignored test as an executed test. The
assignment assertion checks completeness and disjointness before any execution.

For hosted evidence, download all eight OS artifacts and the support artifact.
Create a JSON bundle with `shards` holding the eight records, `reference` holding
the unsharded inventory array, and `support` holding the support record enriched
with the support job's Jobs API `started_at` as epoch `started`.
Set `workflowStarted` to the workflow's `created_at` epoch and `completed` to the
aggregate's Jobs API `completed_at` epoch. Run:

```sh
python3 scripts/ci-shards.py --audit bundle.json --output audit.json
```

The audit rejects missing/duplicate shards, targets or results, failed binaries,
reference/listing/ignored-set differences and absent/failed support or doctests.
Its elapsed time starts at the workflow, including scheduling, checkout, setup,
build, support and fan-in. It does not mistake aggregate-job duration for the
required result's latency. Record queue delay separately using Jobs API
`started_at` and workflow `created_at`. Build timing in shard records starts at
the no-run build; use Jobs API times for checkout/setup and support durations.

## Timing evidence

The supplied baseline is Ubuntu run 41726268369. It is ticket evidence, not a
measurement repeated in this container. Tests are unchanged, so these numbers
are placement estimates rather than a claim of faster individual tests.

| Binary | Supplied baseline seconds | Local shared-host seconds | Hosted after seconds |
| --- | ---: | ---: | --- |
| golden_v8_v8w | 626 | 1490.9 | pending |
| manta_engine unit tests | 425 | 942.3 | pending |
| determinism_hsmm | 239 | 1089.8 | pending |
| multi_input_hpsdr | 180 | 156.9 | pending |
| listen_inputs | 122 | 183.5 | pending |
| rtty | 120 | 176.4 | pending |
| soak_ci | 120 | 120.3 | pending |
| multi_input | 118 | 109.8 | pending |
| rtty_detector | 102 | 99.4 | pending |

Hosted Ubuntu/macOS setup, cache state, build, support, critical path and run
URLs remain pending. Local results below are container observations and cannot
establish hosted latency. The container has a six-core CPU quota; it is not a
GitHub hosted runner. Local warmed-build results must not be compared as a
speedup against the supplied hosted baseline.

## Local implementation checks

On Linux x86_64 at main `18eaa0d5a924bf608c0e39bcf7c382aa21545355` plus
this change, `cargo build --all-targets` passed in 4m 12s. Formatting made no
Rust changes. Workspace lint autofix passed in 9m 45s while test processes
shared the container; it made no Rust changes. The 115 shard, workflow and
trust-boundary regression tests passed, including the real Cargo fixture.

Default workspace discovery found 152 test executables, 4,739 test names and
46 ignored names. Both ordinary and ignored listings matched Cargo's serial
listing per executable, not only as a total count. Every target is assigned
once across the eight shards. Cargo verbose execution of the library target
confirmed manifest/package variables and dynamic-library paths. That comparison
prompted preserving all Cargo package metadata variables and the toolchain
library directory; the fixture checks runtime package variables, cwd and CLI
execution. The three-run HSMM test and golden test bodies are unchanged.

All eight local shard selections completed: six passed, two failed. The failures
were the engine doctor prompt-return assertion and two HPSDR no-progress cases.
Golden and three-run HSMM determinism passed. Every one of the 152 executables
ran once. The bundle correctly fails the success audit because three tests failed
across two binaries. Warm no-run build durations ranged from 0.30 to 1.29 seconds.
These warmed builds exclude the earlier 4m 12s all-target build.

Local shard execution uses four concurrent driver processes on one six-core
quota container. This is a stress probe, not the CI layout of one host per
shard. It exposed HPSDR no-progress timeouts and the engine doctor
prompt-return assertion under contention. These failures remain in the raw
shard evidence and must not be counted as an all-green hosted run. The final
serial repository gate is a separate check. Hosted timing and resource-isolated
shard reliability still require the rollout below.

## Rollout and timeout restoration

After publication, collect a representative decoder-only PR run and merge-group
run on hosted Ubuntu and macOS, then repeat once per OS. Record run URLs, OS,
runner type, cache state, queue delay, setup/build/support/critical-path durations
and the nine binary durations above. Include a cold-cache observation where
available. Audit the old and new inventories and ignores, explicit doctests and
feature/script coverage. Confirm the unchanged three-run HSMM equality and
golden assertions under the actual graph.

Every required result must finish within 1,200 seconds of workflow creation,
including Windows if required by the live ruleset, Supply Chain and Codex review.
If a binary or repeated build cost exceeds budget, keep the ticket incomplete,
rebalance weights or increase shards and measure again. Keep all assertions.

Only after measurements pass may an operator with repository administration
permission read ruleset 18658768, change only
`merge_queue.parameters.check_response_timeout_minutes` from 90 to 60, and read
it back. Preserve all other rules and parameters. Record both readbacks and run
URLs, then verify the next merge-group run has no `checks_timed_out`. This
implementation does not claim that setting has changed. Roll back scheduling
if correctness regresses; retain the 90-minute stopgap until a measured
replacement is ready.
