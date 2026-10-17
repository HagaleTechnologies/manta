# MAN-235: doctor hints a longer run, and `--strict` puts the verdict in the exit code

`manta doctor` stops after 10 s of source time by default, but the
project's own demo signal (`manta gen v1`) reaches its first confirmed spot
between 20 s and 21 s. A default run on a healthy chain therefore read
`WEAK_NO_DECODE` and blamed "mode, WPM, grammar gate". Doctor also exited 0
for every verdict, so a script could not act on what it heard.

Doctor now prints a `hint:` line after a short `WEAK_NO_DECODE` verdict, and
a new `--strict` flag makes every verdict other than `DECODING` exit 3. The
code is `fmt::doctor_rerun_hint` and `strict_exit_code` in
`crates/manta-cli/src/fmt.rs` and `crates/manta-cli/src/main.rs`. The
operator runbook is [setup-checks](../RUNBOOKS/setup-checks.md).
`manta_engine::doctor()`, `DoctorReport`, `Verdict`, `Verdict::summary()`
and `MIN_DURATION`/`MAX_DURATION` are unchanged.

## Decisions

- **D1 — A hint, not a higher default.** The owner decided on 2026-10-05 to
  keep the 10 s default and add a rerun hint. Measured on `manta gen v1`
  with `--json`: `--duration 20` reads `WeakNoDecode` with 0 spots and
  `--duration 21` reads `Decoding` with 1 spot. A default high enough to
  clear that (21 s or more) would add 11 s or more to every live-source run,
  where source time is wall-clock time. The hint costs a live run nothing,
  and the suggested `--duration 60` rerun of the demo file takes about 4.5 s
  of wall-clock time, because file replay is not paced.
- **D2 — When the hint shows, and where.** The line appears when the verdict
  is `WEAK_NO_DECODE` and the requested `--duration` is under 60 s. It
  reads: `hint: a 10.0 s run can end before a working signal is confirmed as
  a spot; a clean signal can need 20 s or more. Rerun with --duration 60
  before looking for another cause.`, with the observed duration in place of
  10.0. It goes to stdout, directly after `verdict:`, in human output only;
  `--json` is unchanged (its consumer already has `duration` and
  `verdict`). It keys off the requested duration because a rerun at 60 s
  changes something only when the run asked for less: a WAV file shorter
  than 60 s shows the hint once, and the suggested rerun then shows none.
  `Verdict::summary()` is untouched, so the verdict wording MAN-270 pins
  ([2026-10-10-man270-cli-output-style](2026-10-10-man270-cli-output-style.md))
  still holds, and a genuinely rejected signal on a 60 s run is not told to
  rerun longer.
- **D3 — `--strict` exit codes.** Supersedes MAN-126 D2
  ([2026-10-10-man126-doctor-setup-checks](2026-10-10-man126-doctor-setup-checks.md))
  only for runs with `--strict`; without it, every verdict still exits 0.

  | Code | Without `--strict` (unchanged) | With `--strict` |
  |---|---|---|
  | 0 | No check failed, whatever the verdict | No check failed and the verdict is `DECODING` |
  | 1 | A check failed, or doctor could not start | Same, whatever the verdict: a failed check takes precedence |
  | 2 | Command-line usage error | Same |
  | 3 | — | No check failed; the verdict is `NO_SIGNAL`, `NOT_CW`, `ACTIVITY_NO_SNR`, `NOISY_NO_DECODE` or `WEAK_NO_DECODE` |

  Code 3, not 1, so a strict script can tell a broken setup (1) from a quiet
  band (3) without parsing JSON; 2 belongs to clap. One code for every
  non-`DECODING` verdict, as `manta status` (0/1/2) and `manta health` (0/1)
  group codes by meaning; a script that needs the exact verdict reads
  `--json`'s `verdict`. `--strict` adds no output line and no `error:` line,
  so MAN-131's "digital silence is a warning and doctor reports
  `NO_SIGNAL`" contract holds.
- **D4 — Every verdict names its code.** `strict_exit_code` matches every
  `Verdict` variant with no wildcard arm, so a new verdict does not
  compile until it chooses a code, and
  `doctor_help_lists_the_strict_code_for_every_verdict` fails until the
  `--strict` help names it. MAN-313's `NotCw` took code 3 that way.
- **D5 — CLI only.** `--strict` has no config key and no `MANTA_*`
  variable, like `--json` and `--duration`: it is a choice about how one run
  reports, not about the node.

## Not done, and follow-ups

- No change to the `--duration` default (D1).
- No per-verdict exit codes.
- The wiki's field-testing page still tells readers to pass `--duration`
  and describes the verdicts correctly, so it is not edited here.
