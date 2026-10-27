# MAN-393: a one-entry `[[input]]` already loads and takes `MANTA_INPUT_*` like `[input]`

Date: 2026-10-26. Ticket: MAN-393, deferred from PR #313 (MAN-13) review
threads [r4300924066](https://github.com/HagaleTechnologies/manta/pull/313#discussion_r4300924066)
(`config.rs` line 764) and [r4300924073](https://github.com/HagaleTechnologies/manta/pull/313#discussion_r4300924073)
(line 1046) under `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`.

## Context

MAN-13 D10 makes a one-entry `[[input]]` array the single form. The two
Codex P2 findings read `take_input_array` and `apply_env_overlay` on their
own: the first refuses an unnamed entry and asks an `audio` entry for
`center_freq_hz`; the second refuses every `MANTA_INPUT_*` variable when
`input` is an array. Neither sees a one-entry array. `load()` replaces a
one-entry array whose entry is a table with that table, before the env
overlay and before the array/table dispatch. The reviewed commit
`678bc602` already did this, and `main` still does. So the ticket's four
scenarios held at both: an unnamed one-entry `audio` array loads; a named
one needs no `center_freq_hz`; `MANTA_INPUT_FREQ_CORRECTION_PPM=1.25`
sets a one-entry array's correction to 1.25 ppm; and with two entries the
variable is refused with `cannot say which of the 2 [[input]] entries it
sets`.

## Decision

No behaviour change. The one-entry rule stays in one place, `load()`.
D10's per-entry rules (names required, a type on each entry,
`center_freq_hz` on an `audio` entry, `MANTA_INPUT_*` refused) apply with
two or more entries, as `docs/CONFIG.md` already says. Making
`take_input_array` or `apply_env_overlay` length-aware would repeat the
rule, and would move `input = [1]` with a `MANTA_INPUT_*` variable from
today's `cannot say which of the 1 [[input]] entries` to a different
error. Each of the two functions now carries a comment naming the
collapse, where the misreading happened.

## Consequences

- Config format, error text and runtime behaviour are unchanged.
- Regression tests pin the collapse: `one_entry_input_array_is_the_single_form`,
  `a_named_one_entry_audio_input_array_needs_no_center_freq_hz`,
  `manta_input_env_applies_to_a_one_entry_input_array` and
  `manta_input_env_is_refused_with_two_or_more_inputs` (now also at two
  entries) in `crates/manta-cli/src/config.rs`, and
  `manta_input_env_reaches_run_through_a_one_entry_input_array` in
  `crates/manta-cli/src/main.rs`, which takes the one-entry array through
  `resolve()` to `run`'s single-receiver path. With the collapse disabled,
  all but the two-entry test fail with the errors the findings described.
- The MAN-13 record is not rewritten.
