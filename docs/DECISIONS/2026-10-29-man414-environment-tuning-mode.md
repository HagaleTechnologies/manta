# MAN-414: environment overrides switch between frequency and bands

Date: 2026-10-29. Ticket: MAN-414, deferred from PR #345's MAN-310 review
at `151f1c77`, thread `PRRT_kwDOTQvU8M6t-g3V`, under the
[review convergence policy](2026-08-07-pr-review-convergence-policy.md).

## Context

The environment overlay replaced values one key at a time. A file's
`freq_hz` survived `MANTA_INPUT_BANDS`, and file `bands` survived
`MANTA_INPUT_FREQ_HZ`. Both reached the existing selector-conflict check,
so an operator could not switch modes without editing the file. Removing
file bands alone would also leave their band-only `tuning_range_hz`.

## Decision

Before inserting environment values, the shared loader removes the
inherited conflicting selector from the single input table:

- Nonempty `MANTA_INPUT_BANDS` removes file `freq_hz`.
- Nonempty `MANTA_INPUT_FREQ_HZ` removes file `bands` and, when those bands
  existed, their file `tuning_range_hz`.

The loader then inserts every explicit environment value as before. Two
nonempty environment selectors still conflict and the error names both.
An explicit environment range without bands still fails validation, as
does an orphan file range with a same-key frequency override. Empty
values count as unset. Unrelated file settings and environment attribution
survive the switch.

This applies to `[input]` and the one-entry `[[input]]` form normalized by
`load()`. The multiple-input ambiguity guard and input-type replacement
rule run first. The existing validators keep their rules. Frequency is
still in Hz, numeric band centres are still in kHz, and tuning changes
remain restart-only. This refines environment precedence from
[MAN-310](2026-10-17-man310-band-plan.md); band parsing and stream planning
are unchanged.

## Verification

`config.rs` unit regressions cover both directions and single-input forms,
raw-key removal, retained name/host/calibration settings, environment
attribution, dual selectors, empty and invalid values, and explicit or
orphan tuning ranges. The two `config_command.rs` regressions
`environment_bands_replace_the_file_frequency` and
`environment_frequency_replaces_the_file_bands_and_range` check the real
CLI's exit status, frequency, attribution and plan notes without opening
a receiver.
