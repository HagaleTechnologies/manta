# MAN-518: build the test executables once per OS and run each shard from a prebuilt slice

Required Ubuntu and macOS CI used to compile every test executable on each of
the eight shards of each OS (`cargo test --workspace --locked --no-run` in
`scripts/ci-shards.py`), then run about an eighth of them. In run 42339317970
(push to main at 915db0e) that was 2,111 compile seconds on Ubuntu and 2,543 on
macOS, 35 % and 44 % of the shards' script time.

## What changed

- `ci-full.yml` has an `ubuntu-build` and a `macos-build` job. Each runs
  `python3 scripts/ci-shards.py --count 8 --build-bundles test-bundles`, which
  compiles once, lists every executable once (`--list` and `--list --ignored`),
  assigns all eight groups with the MAN-498 `assign()`, and writes one
  `slice-<i>.tar.gz` per shard. The job uploads its evidence as
  `build-<os>` and the slices as `test-bundle-<os>-0` to `test-bundle-<os>-7`.
- `ubuntu-shards` and `macos-shards` need their build job. Each shard downloads
  only `test-bundle-<os>-<shard>` and runs
  `scripts/ci-shards.py --from-bundle test-bundle/slice-<shard>.tar.gz`. The
  shards no longer install the Rust toolchain, check its pin or restore
  rust-cache, and no shard step runs `cargo`, `rustc` or `rustup`.
- `test (ubuntu-latest)` and `test (macos-latest)` keep their names and now
  need three jobs: `<os>-support`, `<os>-build` and `<os>-shards`. The verdict
  passes only when all three succeeded (`length == 3`). A failed build skips the
  shards, and a skipped dependency fails the aggregate.
- In `--from-bundle` mode each child process sees a temporary directory first on
  `PATH` that holds `cargo`, `rustc` and `rustup` shims. They print
  `MAN-518 tripwire: a shard invoked <tool>` and exit 97, and `CARGO` points at
  the `cargo` shim. No test calls these tools today, so verdicts do not change;
  if one ever does, the shard fails with that message.
- The merge-main step of every test job pins `GIT_AUTHOR_DATE` and
  `GIT_COMMITTER_DATE` to the newest parent's committer time. On a
  `workflow_dispatch` run every job used to create its own merge commit with its
  own clock, so the build job's `manta` binary would embed a different commit
  than the shard's `HEAD`, and `build_identity` would fail. Now every job of a
  run creates the same merge commit. `MANTA_GIT_SHA` was the alternative; it
  turns off the check against git, so it was not used.

## What each slice holds

A slice is a gzip (level 1) tar. Its first member is `shard-plan.json`:
`version`, `index`, `count`, `workspaceRoot`, `targetDirectory`, `gitHead`,
`gitTree`, the full `inventory` (the fields MAN-498 records) and the `selected`
items, each with its working directory (`cwd`, relative to the workspace root),
its Cargo variables (`env`) and its library path entries (`libraryPath`). The
shard computes nothing from Cargo. The other members are the slice's test
executables plus every non-test `bin` executable of a package the slice holds,
which today is `target/debug/manta` for any `manta-cli` executable (the
`CARGO_BIN_EXE_manta` target). The build job stores hard links as regular
files.

Before extracting anything, a shard refuses the slice when its `--shard` or
`--count` differ from the plan, when its working directory differs from
`workspaceRoot`, or when `git rev-parse HEAD` differs from `gitHead`. Every
member other than the plan must be a regular file under the target directory,
with a relative name and no `..`; links, absolute names and other directories
are refused. Extraction uses the `data` filter where Python has it. The shard
then creates `<target>/tmp` (`CARGO_TARGET_TMPDIR`).

## MAN-498 guarantees that still hold

- Whole binaries run with normal libtest threading, so process-local locks stay.
- Listing comes before execution, on the host that runs the tests: the shard
  re-lists its own executables and fails with `listing drift for <key>` if a
  listing differs from the plan's.
- New targets are still scheduled automatically at weight 30.
- The shard record keeps its fields (`version`, `index`, `count`, `started`,
  `completed`, `runner`, `inventory`, `selected`, `results`, `buildSeconds`) and
  adds `gitHead`, `gitTree` and `extractSeconds`. `inventory` comes from the
  build's listing in `shard-plan.json`.
- `--audit` takes the same bundle. It also rejects shard records that name
  different commits, and, given an optional `build` key holding the `build-<os>`
  record, a build whose inventory or `gitHead` differs from the shards'.

## What a shard needs

Checkout and the merge-main step (tests read the source tree through
`CARGO_MANIFEST_DIR`, and `build_identity` asks git for `HEAD`), `libasound2-dev`
on Linux (the `manta` binary links `libasound.so.2`), `python3` and `git`. The
other tools tests spawn (`sh`, `zsh`, `fish`, `mandoc`, `groff`) come with the
runner image. The binaries embed absolute paths (`CARGO_MANIFEST_DIR`,
`CARGO_BIN_EXE_manta`, `CARGO_TARGET_TMPDIR`), which stay valid because every
hosted job of one OS has the same checkout root (`/home/runner/work/manta/manta`,
`/Users/runner/work/manta/manta`).

## Artifacts and cache

Measured in a 2-CPU container at 915db0e: the eight slices hold 1.7 GB in all;
slice 0 (`golden_v8_v8w` plus `target/debug/manta`) is 36 MB and slices 1 to 7
are 185 to 297 MB. Packing all eight took 28 s with one thread per slice. The
build job uploads them with `compression-level: 0` (they are already
compressed) and `retention-days: 1`; they are useless after the run.

The implementation check, in a 6-CPU container at 915db0e with a warm
`target/`: the default mode's `--inventory-only` and `--build-bundles` gave the
same inventory (160 executables, 4,931 test names, 46 ignored). Packing took
10 s, and the slices were 35 MB for slice 0 and 184 to 296 MB for the others
(1.6 GiB in all). With `target/` moved away and a `PATH` holding only `python3`
and `git`, `--shard 5 --from-bundle slice-5.tar.gz` exited 0 in 314 s:
25 binaries, all exit 0, 1,822 passed, 10 ignored, extraction 3.9 s, and no
tripwire line.

The build jobs restore and save rust-cache under `shared-key: ubuntu-shards`
and `shared-key: macos-shards`, the keys the shards wrote, so the first runs
start from warm dependencies. The build job is now the only writer, and it
still saves only on a push to main.

## Latency and measurement

The required check does not get faster: shard 0's `golden_v8_v8w` (about 700 s)
bounds it, and that is a separate ticket. The build job now runs before the
shards, so `test (ubuntu-latest)` may complete up to about 2.5 minutes later.
Runner time falls by roughly 27 minutes per run on Ubuntu and 32 to 39 on
macOS, and a compile error shows once per OS instead of eight times.

Compile seconds per OS per run = the build record's `buildSeconds` + the sum of
the shard records' `buildSeconds` (`0` on a prebuilt shard). The same formula
gives the "before" column from the old records.

| | Before (run 42339317970) | Estimate | Hosted after |
| --- | ---: | ---: | ---: |
| Compile seconds, Ubuntu | 2,111 | 212 to 303 | pending |
| Compile seconds, macOS | 2,543 | 154 to 488 | pending |
| Sum of Ubuntu build and shard job durations | 6,243 s | 4,500 to 4,650 s | pending |
| Sum of macOS build and shard job durations | 6,045 s | 3,700 to 4,100 s | pending |
| `test (ubuntu-latest)` after run creation | 1,016 s | 1,000 to 1,170 s | pending |
| `test (macos-latest)` after run creation | 1,063 s | 870 to 1,270 s | pending |

Fill "Hosted after" from the first push-to-main run of this pipeline. If either
`test (...)` completes more than 1,200 s after run creation in two consecutive
push runs, file a follow-up ticket.

## Re-runs

GitHub's documentation does not say whether "Re-run failed jobs" lets
`actions/download-artifact` see an earlier attempt's artifacts, and slices
expire after one day. If a re-run shard cannot download its slice, the
download step fails closed; use "Re-run all jobs".

## The default mode stays

`scripts/ci-shards.py` with neither `--build-bundles` nor `--from-bundle` still
builds, lists and runs one shard as MAN-498 described. A pull request's
dispatched run reads main's `ci-full.yml`, so the run that first carries this
script uses that mode, and the MAN-498 local reproduction commands use it too.

## Local reproduction

```sh
cargo test --workspace --locked --no-run
python3 scripts/ci-shards.py --inventory-only --output legacy.json
python3 scripts/ci-shards.py --count 8 --build-bundles bundles --output build.json
# compare sorted((key, tests, ignored)) of the two inventories, then move target/
# away and run one slice with no compiler on PATH (PATH holding python3 and git):
python3 scripts/ci-shards.py --count 8 --shard 5 \
  --from-bundle bundles/slice-5.tar.gz --output shard-5.json
```

## Auditing hosted evidence

Build the MAN-498 bundle from the eight `shard-<os>-<i>` records, the `support`
record and the timing fields, set `reference` to an unsharded inventory, and add
`build` holding the `build-<os>` record. Then run
`python3 scripts/ci-shards.py --audit bundle.json --output audit.json`. To
compare with an older run of the same source, compare the new records' `gitTree`
with the older merge commit's tree (`git rev-parse <merge>^{tree}`).
