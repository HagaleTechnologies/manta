# MAN-517: shard the Windows test leg like the Unix legs

Required Windows CI now has the job shape MAN-498 gave Ubuntu and macOS. The
serial `test` job is gone. In its place `ci-full.yml` has three jobs:

- `windows-support` runs the old job's steps in order, with
  `cargo test --workspace --doc --locked` (step id `doctests`) in place of the
  serial `cargo test --workspace --locked`. It then writes and uploads
  `support-windows` evidence, as the Unix supports do.
- `windows-shards` runs eight legs. Each one runs
  `python scripts/ci-shards.py --count 8 --shard N`, which builds the test
  executables once on the leg and runs its share of whole binaries.
- `windows-aggregate` carries the display name `test (windows-latest)`. It
  needs both jobs above and passes only when both succeeded. A failed,
  cancelled, skipped or missing dependency fails it.

The check name, MAN-371's `create-ci-check` and the relay's lookup by display
name are unchanged. The relay now needs `windows-aggregate` in place of `test`.

This changes scheduling. It is not a measured hosted outcome: every timing
below is either measured on the old serial job, estimated from those logs, or
pending.

## Why eight shards, and no Windows weights column

The research ran `ci-shards.py`'s own `assign()` over the real 160-executable
inventory and scored each shard with the per-binary seconds from two hosted
Windows logs of the old serial job (measured inputs, estimated shard loads).

| Runner class | Shard loads in seconds, shard 0 first | Largest non-golden shard |
| --- | --- | ---: |
| slow (run 42339317970) | 874, 825, 651, 359, 644, 414, 575, 403 | 825 s |
| fast (run 42336347294) | 425, 385, 372, 286, 373, 288, 329, 217 | 385 s |

Makespan (largest shard load) on the slow runner, estimated the same way:

| Shards | Committed weights | Windows column built from the same log |
| ---: | ---: | ---: |
| 6 | 1,094 s | 874 s |
| 8 | 874 s | 874 s |
| 10, 12, 16 | 874 s | 874 s |

At eight shards `golden_v8_v8w` sits alone on shard 0 and no other shard is
heavier, so more shards add builds and gain nothing, and six miss that floor.
A Windows column gives the same 874 s makespan at eight shards: Windows
binaries cost about 1.22 times their Ubuntu time, with the same order of large
binaries. `scripts/ci-test-weights.json` is unchanged, so MAN-498's Unix
assignments do not move.

Estimated critical path (shard 0): about 47 s set-up, 601 s build, about 20 s
listing, 874 s for `golden_v8_v8w`, about 35 s for post steps and the
aggregate. That is about 26 minutes on the slowest runner seen and about 15 on
a fast one, against 49 to 92 minutes measured for the serial job over 29 green
runs on 2026-11-05 to 2026-11-07. These are estimates, not measurements of the
new graph.

## Decisions

- **Windows shards build per leg.** They do not consume a build-once artifact.
  That would add an upload, a download and an extract to the critical path,
  and this ticket is about latency. Windows minutes are free on this public
  repository. A bundle path is also unproven on Windows: tripwire shims would
  need `.exe` files. A follow-up can move Windows to build-once once hosted
  Windows timings exist.
- **`timeout-minutes: 60` on `windows-shards`.** A timeout guards against a
  stuck job; it is not the 35-minute target. A cold cache or a slower runner
  could push a healthy shard past 35, and a timeout there would turn a slow
  green run red, fail the release gate and open a main-red alert. Sixty is
  1.7 times the target, the ratio MAN-498 used (35 against 20).
- **`shared-key: test` on the shards' rust-cache step.** rust-cache keys on
  the job id. `test` was the serial job's id, so the shards restore its warm
  `v0-rust-test-Windows_NT-*` entry instead of building every dependency cold.
  `windows-support` keeps its own job-id key, like the Unix supports, so no
  two different jobs race to save one entry. Neither job may set a job-level
  variable starting `CARGO`, `CC`, `CFLAGS`, `CXX`, `CMAKE` or `RUST`: that
  changes rust-cache's environment hash and so the key.
- **The shard step uses the default shell (pwsh) and `python`.** The serial
  `cargo test` step had no `shell:`, so its tests inherited pwsh's
  environment. Running the script from Git Bash would hand the tests MSYS
  variables they never had. `python` is the interpreter name on these runners.
- **No early hosted run from the branch.** Dispatching `ci-full.yml` at the
  branch would post GitHub Actions `test (...)` check-runs beside
  hagale-agent's, which MAN-243 forbids, and `create-ci-check` and the relay
  need the main-only `codex-review` environment. The first hosted run of the
  new graph is the PR's merge-group run.

## `ci-shards.py` on Windows

- `dylib_path_variable()` returns `PATH` on Windows,
  `DYLD_FALLBACK_LIBRARY_PATH` on macOS and `LD_LIBRARY_PATH` elsewhere, as
  Cargo's `dylib_path_envvar` does. `child_env()` takes the platform as an
  argument so a Linux test covers the Windows branch.
- Every text-mode subprocess call and file read decodes UTF-8. The Windows
  locale codec is cp1252; Cargo's JSON and libtest's listing are UTF-8.

Scenario 2 (one failing binary does not hide the others) needs no new flag.
The shard loop has no `break`, so a failing binary changes only the exit code
and the evidence. The planning phase reproduced the old behaviour on a scratch
crate with `a_fail` and `b_ok`: plain `cargo test` stopped after `a_fail` and
never ran `b_ok`; `ci-shards.py` ran both and exited 1. The test
`test_a_failing_binary_does_not_stop_its_shard` in
`scripts/tests/test_ci_shards.py` locks this in. The only Cargo test command
left on Windows is the doctest step, and the workspace has no doctests.

## Costs

- Each run has 34 jobs instead of 25.
- Windows runner minutes per run rise from about 92 to about 210 on a slow
  runner (eight builds), estimated; free on a public repository.
- The `windows-support` cache entry adds about 450 MB. The repository already
  held about 10.6 GB of Actions cache when measured.

## What did not change

`release-publish.yml` and its Windows static-CRT assertion, `create-ci-check`,
the three check names, `scripts/check-release-ci.py`,
`scripts/required-check-sources.py`, `scripts/ci-test-weights.json`, and
`.catalyst/catalyst.toml`'s serial gate (still the local reference command).

## Hosted verification

Nobody can measure the ticket's wall-clock until the new graph runs on GitHub.
For each of the first five green push runs of `ci-full.yml` on main, record
the time from workflow creation to the last Windows job's end (the jobs API:
`repos/HagaleTechnologies/manta/actions/runs/<run>/jobs`, Windows jobs only),
the runner class from shard 0's `golden_v8_v8w` seconds (above 600 s is
slow), and each Windows job's queue delay. For at least one run, download the
`shard-windows-latest-*` and `support-windows` artifacts and run
`python3 scripts/ci-shards.py --audit <bundle> --output audit.json`, which
must print `"coverage": "complete"`; compare its executable count with the
last serial run's 160.

| Push run | Minutes to last Windows job | Runner class | Coverage audit |
| --- | --- | --- | --- |
| 1 | pending | pending | pending |
| 2 | pending | pending | pending |
| 3 | pending | pending | pending |
| 4 | pending | pending | pending |
| 5 | pending | pending | pending |

If any run takes 35 minutes or more, the ticket is not done. File the
follow-up the numbers point to: a Windows weights column if a non-golden shard
is longest, the golden split if shard 0 is, the cache if the build was cold.
Do not raise the timeout to hide it.

## Not done

- Windows build-once.
- Splitting `golden_v8_v8w`; the next floor would be `manta_engine` at 632 s.
- Making `test (windows-latest)` a required check (MAN-371 O1).
