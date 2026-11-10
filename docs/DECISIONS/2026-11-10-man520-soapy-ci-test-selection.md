# MAN-520: CI's test-soapy leg runs only the tests the soapy feature can change

Date: 2026-11-10. Ticket: MAN-520.

## Context

`test-soapy` in `.github/workflows/ci-full.yml` is not a required check.
It ended with `cargo test -p manta-input -p manta-cli --features soapy
--locked`, which built and ran all 82 test executables of both crates with
the feature on. Over the 96 hosted runs read for the research (2026-11-06 to
11-10), the job's median was 40.5 min on Ubuntu (p90 44.0, max 47.3) and
38.9 min on macOS (p90 41.4, max 43.7). The `cargo test` step was almost
all of it: a median of 2,332 s on Ubuntu and 2,198 s on macOS over 12 green
push and merge-group runs, against 14 to 20 s for clippy. Between
2026-10-21 and 11-07, 20 red push and merge-group runs had `test-soapy` as
their only red job.

Most of that time re-ran tests the feature cannot change. Since MAN-550,
`crates/manta-cli/src` names `feature = "soapy"` on 4 lines, all in or
beside `soapy_opts.rs`
([MAN-550 decision](2026-11-08-man550-soapy-opts-module.md)), and
`crates/manta-input/src` gates its `soapy` module.

## Decision

The job keeps its clippy line, which still compiles every target of both
crates (and the `iq_probe` example) with the feature, and replaces the one
`cargo test` line with two:

```sh
cargo test -p manta-input --features soapy --locked --lib soapy
cargo test -p manta-cli --features soapy --locked --no-fail-fast --bin manta \
  --test build_identity --test cli --test completions_and_man_pages \
  --test config_command --test docker_image --test docs_consistency \
  --test field_node_runbook --test operator_guides --test output_style_guard \
  --test packaging_examples --test source_check
```

- **Two commands.** Cargo applies a test-name filter to every target of one
  invocation, so `manta-input`'s `soapy` filter needs a command of its own.
  It runs the 13 `manta-input` unit tests whose path contains `soapy`: the
  12 in `soapy::tests`, which `release-publish.yml`'s `build-soapy` runs,
  and `devices::tests::soapy_conversion_preserves_every_backend_argument`,
  which that release filter misses. These 13 are exactly the lib tests that
  exist only in a soapy build.
- **Whole test roots, no name filter, on the `manta-cli` side.** A planted
  break in `--source soapy` option parsing (mutation M1 below) was caught
  by four tests in `cli`, and two of them are not named for soapy
  (`hpsdr_host_conflicts_with_every_other_source_flag` and
  `ka9q_group_conflicts_with_every_other_source_flag`). A name filter would
  have dropped them.
- **The roots are the `crates/manta-cli/tests/*.rs` files that mention
  soapy, case-insensitively**, plus one extra:
  `completions_and_man_pages`, which never says "soapy" but reads every
  long flag from `--help`, so it checks the four `--soapy-*` flags.
  No `crates/manta-input/tests/*.rs` file mentions soapy.
- **`--bin manta` stays.** Mutation M3 below is caught only by
  `soapy_opts::tests::from_file_succeeds_only_in_a_soapy_build`, a unit test
  in the binary. It costs about 27 s.
- **`--no-fail-fast`** on the `manta-cli` command, so one red root does not
  hide another in a short leg.
- **No `timeout-minutes`.** A timeout turns a slow run red, and any red job
  in a tagged commit's push run blocks a release
  (`scripts/check-release-ci.py`). The 15-minute target is read from the
  PR's own dispatched `ci-full` run and later push runs, not enforced.

### The guard

`SoapyTestSelectionTests` in `scripts/tests/test_ci_gates.py` runs in the
required `*-support` jobs. It requires the `manta-cli` command's `--test`
set to equal the grep set plus `SOAPY_EXTRA_TESTS`, in both directions, and
reports missing and extra names separately. It also requires the
`manta-input` command and the clippy line verbatim, allows only `-p`,
`--features`, `--bin`, `--test`, `--locked` and `--no-fail-fast` in the
`manta-cli` command (so no name filter and no `--`), and requires
`--bin manta`, `--locked` and `--no-fail-fast`. It counts a
`tests/<name>/main.rs` target like a `tests/<name>.rs` one, and fails when
shared test code (such as `tests/common/`) mentions soapy, because a text
grep cannot tell which roots use it. On the `manta-input` side it fails if
any integration test mentions soapy, or if a unit test or module in `src`
is gated on the feature but has no `soapy` in its test path, since
`--lib soapy` would skip it. Any `cargo test` step other than the two
above also fails it.

The match is exact on purpose. A new test file that mentions soapy fails
the required check until `test-soapy` lists it, and the list cannot grow
back into the whole suite without the guard noticing. The cost: adding the
word soapy to a test file means editing `ci-full.yml`, a CODEOWNERS path.
That is rare, and a CI change should get that review anyway.

## Consequences

Measured in a 2-CPU container on `acbc00f`, with SoapySDR 0.8.1 unpacked
into a private prefix: the `manta-input` command took 85 s with a cold
compile (13 passed), and the `manta-cli` command 233 s to build and 123 s
to run (the `manta` binary 947 passed, `cli` 105, `docs_consistency` 117,
`config_command` 77, the other eight roots 99). The hosted duration is not
measured yet; the PR's `ci-full` run is the first reading, and the target
is under 15 minutes on both legs.

Mutations, each run against the selection and reverted:

- **M1**, `#[arg(long = "soapy-drv", ...)]` in place of `soapy-driver` in
  `soapy_opts.rs`: exit 101, `cli` 4 failed and `source_check` 1 failed
  (`a_source_path_with_soapy_driver_is_a_usage_error`). This is the
  ticket's second scenario: a soapy-only regression still fails the leg.
- **M3**, `SoapyOpts::from_file` swapping frequency and rate: caught only
  by the `manta` binary's unit test above.
- **M4**, `SoapySdrIqSource::open` swapping its sample-rate and frequency
  arguments: every soapy test passes. SoapySDR's `type=null` device accepts
  any values, and `open_succeeds_against_the_null_device` asserts only
  `is_ok()`. No test outside `manta-input` calls `manta_input::soapy`, so
  the old full suite misses it too. This is a gap in the existing tests,
  not in the selection, and MAN-520 does not close it.

`--lib` runs no doctests. No doc example in `manta-input` sits behind the
feature today, and the default suite's doctest run cannot see one that
does. If one is added, it needs its own `--doc` command in `test-soapy`.

`CONTRIBUTING.md` gives the same commands for local SoapySDR work. The
`release-publish.yml` `build-soapy` test line, and the job id, matrix and
setup steps of `test-soapy`, are unchanged.
