# MAN-392: calibrate reports a standalone receiver's measurement beside several inputs

Date: 2026-10-25. Ticket: MAN-392, a deferred Codex P2 from MAN-13's PR #313
(on `crates/manta-cli/src/calibrate_cmd.rs:493`, deferred after the third
review round under `docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md`).

## Context

MAN-13 D11 lets a source flag (`--source`, `--kiwi-host`, `--ka9q-group`,
an `--hpsdr-*` selector, …) name a standalone receiver beside several
`[[input]]` entries; the entries are then ignored and `--input` is not
needed. Input resolution honoured that, but after the measurement
`calibrate_cmd::run` read the file's own `freq_correction_ppm` with
`config_edit::set_freq_correction_ppm(text, 0.0, None)`. That call refuses
an unnamed `[[input]]` array, so the command threw the measurement away.

Reproduced on `40d275a` with two entries `20m`/`40m` and
`manta calibrate --duration 12 --config manta.toml --source-iq --source ref.wav`
on a 2.5 ppm fixture, with and without `--json`: exit 1, empty stdout, and
on stderr the `note: --source selects the source; ignoring the 2 [[input]]
entries …` and `measuring …` lines followed by
`error: the config has 2 [[input]] entries; name the one to set
freq_correction_ppm in with --input`. The config was left unchanged.

## Decision

Calibrate reads the file's value only when `resolved.replaced_file_source`
is `None`, i.e. when no source flag replaced the file's receiver. When one
did, the value is never used: `configured_origin` already ignores it, and
every flag that sets `replaced_file_source` is also an input override, so
`save_decision` returns `SaveDecision::Overridden` before it reads
`already_set`.

The key is `replaced_file_source`, not the input-override list. That list
is wider: it also holds `--dial-freq-hz`, `MANTA_INPUT_*`, and `--source`
beside an untyped `[input]`, whose `freq_correction_ppm` still applies to
the receiver the flags name. Skipping on the list would turn that case's
`configured: 1.50 ppm, from …/manta.toml` into `nothing set`.

`config_edit::set_freq_correction_ppm` is unchanged: refusing an unnamed
multi-entry array is right for a save. Discarding its error with `.ok()`
was rejected because it would also hide a parse or type error in the file.

## Consequences

- A source flag beside several `[[input]]` entries now exits 0 and prints
  the usual report (or the `--json` object, `"written":false`), with
  `configured: 0.00 ppm, nothing set` and the existing
  `not saved: input overrides were in effect (--source, --source-iq) …`
  line. The stderr notes are unchanged.
- No file write and no wording change: the lines are the ones a source flag
  replacing a typed `[input]` already printed.
- `--input NAME --write` without a source flag still saves into that entry
  only (`calibrate_write_saves_into_the_named_input_entry`).
- The test is `calibrate_a_source_flag_beside_several_inputs_reports_the_measurement`
  in `crates/manta-cli/tests/calibrate.rs`. The MAN-13 and MAN-127 records
  are not rewritten.

## The HPSDR discovery fakes (MAN-450)

This branch's gate failed in two tests it does not otherwise touch:
`devices::tests::enumerate_hpsdr_finds_a_fake_on_an_ephemeral_port` and
`tests/hpsdr_discovery.rs`. A fake radio's reply `send_to(...).unwrap()`
panicked with EPERM. Discovery sends to `127.255.255.255`, then
`127.0.0.1`, from one client port. The fake's reply to the first probe and
the client's second probe are opposite-direction flows between the same two
loopback ports. When both are new at once, Linux conntrack keeps one and
drops the other, and the dropped send returns EPERM. Whether it happens
depends on timing, so the tests failed intermittently, on the base too.

The MAN-450 decision put the fix on main, outside this change: both
fakes now ignore a failed reply send (manta#450, 62e2d747). An earlier
push of this branch carried the same fake-side fix; merging main dropped
it, so this branch no longer touches `devices.rs` or
`tests/hpsdr_discovery.rs`.
