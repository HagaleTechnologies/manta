# MAN-548: main.rs keeps the clap tree and dispatch; each command's wiring lives in its own module

Date: 2026-11-08. Ticket: MAN-548.

## Context

`crates/manta-cli/src/main.rs` was 15,723 lines at `acaa9f7`. 9,578 of them
were code and 6,145 were one inline `mod tests`. The file held the clap tree,
the dispatch in `run_command`, the 1,212-line `Command::Run` arm, source
opening, `--record-iq` recording, the value parsers, config resolution, a
tokio HTTP client and the spot-server start-up. Eleven sibling modules named
its items through `crate::` 137 times. Every `run`, `doctor` or `soak` change
touched this one file. `start_spot_server` took ten parameters and carried a
`#[allow(clippy::too_many_arguments)]`.

## Decision

The code moved by item, without changing what any line does. `main.rs` keeps
`Cli`, `enum Command`, `run_command` and the blocks listed under "What stays
in main.rs". Eight modules hold the rest:

| File | Holds |
| --- | --- |
| `src/source_spec.rs` | `KiwiOpts`, `SoapyOpts`, `HpsdrOpts`, `Ka9qOpts`, `LiveSourceSpec` and its `impl`, `transform_source`, the `open_*` functions, `FixedCenterFreqSource`, the source warning texts, decimation and raw-IQ helpers |
| `src/recording.rs` | `--record-iq` destination checks, start, finish budget and report |
| `src/cli_parse.rs` | every clap `value_parser` function and its range constants |
| `src/session_epoch.rs` | the session epoch and nonce for replays (`resolve_epoch`, `session_nonce_for_replay_path`, `fnv1a_*`) |
| `src/resolve.rs` | `CliOverrides`, `Resolved`, `resolve`, `select_input`, `spec_from_file`, replay-mode resolution |
| `src/daemon_client.rs` | `manta status`'s client and the tokio `http_get` that `manta health` shares |
| `src/spot_server.rs` | `start_spot_server`, `SpotServerParams`, and the types its signature names (`SpotServer`, `UplinkDryRun`, `SourceInfo`, `RelayConfigs`) |
| `src/run_cmd.rs` | the clap `RunArgs` struct and `run(args: RunArgs)`, the old `Command::Run` arm |

- **`Run` is a newtype variant.** The variant is now
  `Command::Run(run_cmd::RunArgs)`. Its 37 fields moved in order into a
  `#[derive(clap::Args)]` struct. The variant keeps its doc comment and its
  `#[command(visible_alias = "listen", group(...))]` attribute. clap orders
  arguments by declaration, so `--help`, `-h`, the completions and the man
  pages stay byte-identical, with and without the `soapy` feature. The arm is
  `Command::Run(args) => return run_cmd::run(args),`.
- **`SpotServerParams` replaces the allow.** `start_spot_server` takes one
  `SpotServerParams<'_>` with the ten former parameters as fields, in the same
  order and under the same names. The function destructures it on its first
  line, so the body is unchanged. Every call site writes the fields in the
  old argument order, so argument evaluation order is unchanged too.
- **Tests sit beside their subject.** No unit test is inline in `main.rs` or
  in the eight modules. Each module declares `#[cfg(test)] mod tests;` and
  keeps its tests in `src/<module>/tests.rs`. Tests of what stays in
  `main.rs`, and tests that parse through `Cli` or match on `Command`, are in
  `src/tests.rs`. Fixtures that two or more test files share are in
  `src/test_support.rs`.
- **A guard keeps the layout.** `crates/manta-cli/tests/module_layout.rs`
  reads the source text with `syn`, so it also sees `cfg`-gated code that one
  build does not compile. It has four tests:
  `sibling_modules_reach_the_crate_root_only_for_what_main_rs_defines`,
  `main_rs_keeps_its_unit_tests_in_src_tests_rs`,
  `split_modules_keep_their_unit_tests_in_a_sibling_tests_rs` and
  `split_modules_name_the_target_of_every_tracing_event`. The last one reads
  the source as `proc_macro2` tokens rather than a `syn` tree, so it also sees
  events inside other macros' bodies. It exists because a `tracing` event's
  default target is its module path. Moved out of
  `main.rs`, the startup banner printed `INFO manta::spot_server:` instead of
  `INFO manta:`. The six events that moved now name `target: "manta"`.
- **Imports are explicit.** No module uses a glob import of `crate`. A sibling
  names a moved item by its module, for example
  `crate::cli_parse::parse_prior`. A name used only inside a `#[cfg(...)]`
  block is written as a full path at its use site, so no build sees an unused
  import.

### What stays in main.rs

- The signal handlers, shutdown helpers, health and latency helpers,
  listeners and `RunLists`. `SHUTDOWN_DRAIN_DEADLINE` and
  `rt.shutdown_timeout(` are among them, and three readers match those lines
  as text in `main.rs`: `crates/manta-cli/tests/packaging_examples.rs`,
  `crates/manta-cli/tests/field_node_runbook.rs` and
  `scripts/tests/test_packaging.py`.
- `receiver_desc` and the `prepare_*` functions.
- The `Decode`, `Oracle`, `Gen`, `Soak`, `Doctor`, `Calibrate` and other arms
  of `run_command`.
- The crate-level rustdoc `allow`, which covers every module of the binary.

### No lib.rs

Nothing links `manta-cli` as a library. The integration tests spawn the
binary through `CARGO_BIN_EXE_manta`. A `lib.rs` would need a public API and
a second rustdoc `allow`, and its doc comments would start running as
doctests. The split did not need one.

## Consequences

- Sibling references that reach into `main.rs` through `crate::` drop from
  137 to 40. The 40 left name items that stay in `main.rs`.
- A change to `run`, the source opening, the parsers or the status client
  touches its own file.
- `main.rs` is about 4,300 lines and holds no test.

## Follow-ups

1. Move the daemon runtime support (signal handling, shutdown, health and
   latency helpers, listeners, `RunLists`) to a `daemon_runtime.rs`.
   `multi_run.rs` still names 12 of its items, plus `RttyLogState`. This needs
   `packaging_examples.rs`, `field_node_runbook.rs` and `test_packaging.py` to
   read the drain constants from the new file.
2. Move `receiver_desc` and the `prepare_*` functions to `prepare.rs`.
   `calibrate_cmd.rs`, `config_cmd.rs`, `reload.rs`, `source_check.rs` and
   `doctor_checks.rs` make 9 references to 7 of its items.
3. Move the `Decode`, `Soak`, `Doctor`, `Oracle` and `Calibrate` arms of
   `run_command` into their command modules.
4. Replace `resolve_spot_pinned`'s `too_many_arguments` allow.
5. Move inline tests to sibling `tests.rs` files in `track.rs`, `listen.rs`,
   `validator.rs`, `metrics.rs`, `decoder.rs` and `manta-cli/src/config.rs`
   after PRs #536, #537 and #538 land.
