# MAN-573: pancetta-ft8 moves to PAN-441's merge commit, chrono leaves the lock, and option-ext gets an MPL-2.0 exception

Date: 2026-11-24
Ticket: MAN-573 (chore, dependencies)
Status: decided and built; proved on Linux in a container. The Ubuntu,
macOS and Supply Chain required checks run on the pull request, not here.
Extends: [MAN-323](2026-11-01-man323-ft8-ft4-from-band-streams.md) D1 (the
`pancetta-ft8` pin rule). That record stays as written.

## Context

RUSTSEC-2026-0543 marks every chrono version unmaintained, with no known
vulnerability. The owner accepted it in `deny.toml` on 2026-11-18 (manta
#595, the same decision as pancetta PAN-430/431). No manta crate named
chrono. It reached the lock only through `manta-engine`'s optional `ftx`
feature: `pancetta-ft8` pulled `pancetta-core 0.9.6`, which pulled chrono.
`deny.toml` sets `all-features = true`, so cargo-deny saw that path.

Pancetta's PAN-441 (PR #731, merge commit
`d0c2ff4714cf4a4a45e1818e1b820b6dc2c0febf`) moved `pancetta-core` to
`jiff`. As of 2026-11-24 pancetta has tags `v0.9.5` and `v0.9.6` only, and
no `pancetta-ft8-v*` tag.

## Decisions

### D1. Pin PR #731's merge commit, not pancetta `main`

MAN-323 D1 named PR #550 because that was the PR manta needed. The rule now
reads: a `pancetta-ft8-v*` tag when one exists, else the merge commit of the
newest pancetta PR manta needs. PR #731 is that PR, and its merge commit is
the smallest step from the old pin `7f8078b1`. The `Cargo.toml` comment
points here. `cargo update -p pancetta-ft8` was the only lock update. It
moved `pancetta-core` and `pancetta-ft8` to 0.9.7, removed `chrono`, `time`,
`time-core`, `deranged`, `num-conv`, `powerfmt`, `iana-time-zone`,
`iana-time-zone-haiku`, `android_system_properties` and
`core-foundation-sys`, and added `jiff`, `jiff-core`, `jiff-static`,
`portable-atomic`, `portable-atomic-util`, `dirs`, `dirs-sys`, `option-ext`,
`redox_users` and `libredox`.

### D2. Delete the RUSTSEC-2026-0543 ignore

No chrono path remains, so the entry and its comment go rather than being
reworded. `RUSTSEC-2024-0449` stays.

### D3. One crate-version licence exception for option-ext

The new `pancetta-core` depends on `dirs` (added by pancetta PAN-376,
#709). `dirs` pulls `dirs-sys`, which pulls `option-ext 0.2.0`, licensed
MPL-2.0. `deny.toml` gains
`exceptions = [{ allow = ["MPL-2.0"], crate = "option-ext@0.2.0" }]` under
`[licenses]`. The `allow` list does not change. A global MPL-2.0 allow would
fail `test_only_permissive_licenses` and widen the policy for every crate.
Naming one version makes a future `option-ext` release fail Supply Chain
until someone reviews its licence again. `option-ext` reaches manta only
through `ftx`, and release builds use `--features soapy`, so no shipped
binary links it today. cargo-deny 0.20.2 rejects a `reason` key on the
entry, so the rationale is a comment above it.

### D4. A test pins the exception

`DenyPolicyTests.test_license_exceptions_name_one_crate_version` in
`scripts/tests/test_ci_gates.py` asserts the exception list is exactly
`option-ext@0.2.0` with `MPL-2.0`, as
`test_sources_are_crates_io_coppa_and_pancetta` pins `allow-git`.

### D5. A new record, and the owner confirms the exception in review

Records are not rewritten, so this one extends MAN-323 D1 and the MAN-323
record stays as written. It adds no index row; the Decisions Index workflow
does that after merge. `Cargo.toml`, `/Cargo.lock` and `/deny.toml` are
CODEOWNERS paths, so the pull request needs `@thagale`'s approval before
the merge queue takes it. That review is where the owner confirms the
MPL-2.0 exception.

## Verification

- `cargo tree -i chrono --target all --locked --all-features` prints
  ``package ID specification `chrono` did not match any packages``, and
  `grep -c 'name = "chrono"' Cargo.lock` prints `0`.
- cargo-deny 0.20.2 `cargo deny --locked check` prints `advisories ok,
  bans ok, licenses ok, sources ok` and exits 0. With the option-ext
  exception removed it exits 4 on `option-ext-0.2.0`.
- The four `ftx` steps in `scripts/pre-push.sh` pass: clippy with no
  warnings, `--lib ftx` 33 passed, `--test ftx_band_stream` 5 passed,
  `--test ftx_acceptance` 2 passed.

## Follow-ups

- Remove the option-ext exception when pancetta drops `dirs` or makes it
  optional. That change is pancetta's to make.
- Review the exception again if `ftx` enters release builds (an open
  MAN-323 follow-up).
