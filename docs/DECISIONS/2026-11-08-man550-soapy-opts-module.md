# MAN-550: SoapySDR gating in manta-cli lives in one soapy_opts module

Date: 2026-11-08. Ticket: MAN-550.

## Context

`crates/manta-cli/src` named `feature = "soapy"` on 98 lines: 79 in
`main.rs`, 11 in `source_check.rs` and 4 each in `doctor_checks.rs` and
`calibrate_cmd.rs`. (The ticket counted 106, with 12 in `config.rs`; those
four `config.rs` gates are `ftx`, and the config layer was already
unconditional.) Most of the 98 were clap boilerplate: the four `--soapy-*`
fields copied into `run`/`listen`, `soak`, `doctor`, `calibrate` and
`check`, and 16 `cfg_attr` twins that differed only by `"soapy_driver"` in a
`conflicts_with_all` list. The rest gated the data that carries the
settings to the device, so a feature-off build lost `CliOverrides.soapy`,
`LiveSourceSpec::Soapy` and `ReceiverKind::Soapy`, and four tests only ran
with the feature.

## Decision

`crates/manta-cli/src/soapy_opts.rs` is the one module that names the
feature. It holds three `feature = "soapy"` lines; `main.rs` keeps one, on
its feature-off config test (`soapy_input_type_needs_the_soapy_feature`,
which needs `main.rs`'s private test helpers). 98 lines became 4.

- **Two types, not one empty `SoapyOpts`.** The ticket asked for an
  unconditional `SoapyOpts` that is empty without the feature. The
  empty-when-off role went to the clap struct, `SoapyArgs`, which has the
  four flags with the feature and no fields without it, so a feature-off
  build still rejects `--soapy-*` as unknown arguments (a hidden flag would
  not do: clap_complete ignores `hide`, MAN-133). The data struct
  `SoapyOpts` keeps its four fields in every build. An empty data struct
  would have needed an on/off accessor pair in every arm that reads the
  driver, frequency or rate, and the four soapy tests would have stayed
  feature-only. Now `CliOverrides`, `LiveSourceSpec`, `ReceiverKind`,
  `calibrate`'s `input_overrides` and `apply_tune_hz` carry no gate, and
  those tests run in both builds.
- **`and_driver`.** A non-soapy source flag lists its conflicts as
  `conflicts_with_all = crate::soapy_opts::and_driver([...])`, which
  appends `"soapy_driver"` only in a build that has the flag. Dropping the
  id and relying on clap's two-way conflicts would keep the error text, but
  zsh completion prints each flag's own declared list, so
  `manta completions zsh` would change; and `--tune-hz` exists only in
  `calibrate`, so the shared `SoapyArgs` cannot list it.
- **`join_source_selector`.** `check` puts `--soapy-driver` in its
  `source_selector` group through
  `#[command(mut_args(crate::soapy_opts::join_source_selector))]`, so
  `manta check x.wav --soapy-driver d` stays a usage error without a second
  copy of the flags. Without the feature no argument has that id and the
  function changes nothing. `tests/source_check.rs`
  `a_source_path_with_soapy_driver_is_a_usage_error` pins it.
- **Refusal text kept.** Without the feature,
  `SoapyOpts::from_file` (called by `spec_from_file` during `resolve`, so
  `config check` still refuses) and `soapy_opts::open` fail with the
  existing `input.type = "soapy" needs a manta built with --features soapy`,
  which three tests pin.
- **`gain`.** Without the feature nothing reads `SoapyOpts::gain`, so it
  carries `#[cfg_attr(not(feature = "soapy"), expect(dead_code, ...))]`;
  `expect` errors once something reads it.
- **Flatten in place.** Each `#[command(flatten)] soapy: SoapyArgs` sits
  where the four fields sat, after `--kiwi-password`, so help headings,
  man-page SYNOPSIS order and completion order do not move.
- **Guard.** `soapy_opts::tests::only_soapy_opts_names_the_soapy_feature`
  fails if `src` names the feature on 15 or more lines, or anywhere outside
  `soapy_opts.rs` except one line in `main.rs`.

The rule for the next source flag: a new non-soapy source selector lists
its conflicts through `soapy_opts::and_driver([...])`, and a new soapy flag
goes into `SoapyArgs`.

## Consequences

`--help`, `-h`, the five completion scripts, the 20 man pages (git SHA in
`manta.1` masked) and every parse error compared byte-identical to the
pre-change binary in each build: a matrix of 1068 selector invocations per
build across run/listen/soak/doctor/calibrate/check showed 0 differences.
`crates/manta-input` (which owns the driver) and the per-build test gates in
`crates/manta-cli/tests/` are unchanged. No CHANGELOG entry: nothing
operator-visible changed. The `main.rs` split ticket moves the
`use soapy_opts::SoapyOpts;` line and the `crate::soapy_opts::` paths with
the code that uses them.
