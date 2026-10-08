# 2026-10-07 — MAN-76: `manta config check` and `manta config init`

**Status:** Implemented (branch `MAN-76`). Records the design decisions
behind the two commands that let an operator validate and scaffold a config
file without starting the daemon. Builds on MAN-261's single config file
(`docs/DECISIONS/2026-10-06-man261-config-surface.md`), whose follow-ups
list named both commands.

## Context

Before this change the only way to learn whether a `manta.toml` was valid
was to start the daemon with it. `config::load` already rejected unknown
tables, keys and `MANTA_*` variables and out-of-range values with messages
naming the file, table and key, but `run` also rejects some configs that
`load` accepts: an unreadable blocklist or notch file, an `[input]` type
this build cannot open, and (only for `run --config`) a `[server]` with a
sound card or WAV source and no dial frequency. Every one of those failed
only once the operator tried to start the node. No example config listing
every key existed (MAN-75's PR #124 closed unmerged); README's node section
was the only one.

## Decisions

- **D1 — shape.** `manta config check` and `manta config init`, a nested
  clap subcommand: `Command::Config(ConfigCommand)` with
  `ConfigCommand::{Check { config }, Init { out, force }}`.
- **D2 — which file `check` reads.** `--config`, then `MANTA_CONFIG`, then
  `./manta.toml` if it is a regular file, then none. The first two mirror
  `run`; the third makes a bare `manta config check` work and pairs with
  `init`'s default output path. With no file, `check` validates the
  built-in defaults plus any `MANTA_*` variables (an env-only systemd
  deployment is legitimate), says so on stderr, and exits 0. The first
  summary line always names the file and why it was chosen. `run` itself
  gains no `./manta.toml` discovery, so a file found in the current
  directory also gets a stderr note saying to pass it to `run` with
  `--config` or `MANTA_CONFIG`.
- **D3 — what `check` validates.** Exactly `run`'s config stage before its
  first source I/O: `prepare_live(CliOverrides::none(), path, None)`, which
  is `config::load` with the environment, `resolve` (rejects source types
  this build cannot open) and `build_pipeline_config` (reads and parses
  the blocklist and notch files). Reusing it means "check passes" cannot
  drift from "`run`'s config stage passes". `check` adds two errors of its
  own: duplicate non-zero `[server]` ports, which `run` can never bind but
  discovers only after the receiver is open; and unedited scaffold
  placeholders (any `<…>` string value, or `N0CALL` as
  `station_callsign`/`login_callsign`), which would publish spots under a
  fake call or retry a nonexistent host forever. Notes go to stderr and
  never change the exit code: no config file found; `run` will need a dial
  frequency (`[server]`, a sound card or WAV source, no `center_freq_hz`);
  `bind_addr` is unspecified (`0.0.0.0`/`::`), so the password-less metrics
  endpoint is reachable from every interface; `bind_addr` is neither an IP
  literal nor `localhost`. The dial guard is a note, not an error, because
  the same file is valid for `run --dial-freq-hz …`, `soak` and `doctor`.
- **D4 — never call `LiveSourceSpec::is_rf_aware()` from `check`.** It
  opens IQ WAVs to read their sidecar. The dial note's predicate matches
  `AudioDevice | File` with no dial frequency instead, and its wording
  covers the IQ-recording exception.
- **D5 — output contract.** The summary goes to stdout, notes to stderr.
  Exit 0 valid, 1 invalid (the loader's error through `main`'s `Result`,
  unchanged), 2 for a clap usage error. The Kiwi password is shown only as
  `password=set`/`password=none`, and a placeholder-shaped password is
  rejected without echoing it. Output is deterministic: no timestamps,
  and ports print as configured (`0` stays `0`).
- **D6 — summary format.** One line per table, `key=value` tokens named as
  the config keys, in this order: the origin line
  (`<path>: valid (from --config)`, `(from MANTA_CONFIG)`, `(found in the
  current directory)`, or `no config file: valid (…)`), `environment:`
  (the applied `MANTA_*` names, or `none`), `server:` (or `none (manta run
  starts no servers)`; `Option` keys appended only when set),
  `rbn_uplink:` (one line per entry, or `none`), `input:` (per type;
  untyped is `type=unset …`; shared keys appended when set; file paths
  resolved), `spot:`, `detector:` and `decode:`. The last two print only
  the keys actually set, with their written values, then `(other keys
  default)`; printing all 33 values would bury the few that matter.
- **D7 — `Loaded` gains two fields.** `raw`, the document after the
  environment overlay (the summary's set keys and the placeholder scan
  read it), and `env_vars`, the sorted `MANTA_*` names the overlay
  applied. No other loader change.
- **D8 — `init`'s output.** `./manta.toml` by default; `--out <path>`
  writes elsewhere and `--out -` prints to stdout (for packaging and
  `| sudo tee`). Without `--force` the file is opened with `create_new`,
  so an existing file is never replaced; that case exits 1 naming
  `--force` and `--out`. The parent directory must exist. On success
  stdout stays empty and stderr says which file was written and to run
  `manta config check` next. The umask applies; the scaffold holds no
  secret.
- **D9 — every key commented out.** The scaffold lists each key as
  `#key = value` (the postgresql.conf/sshd_config convention), never as an
  active line: a commented-out key follows the built-in default, so a
  later release's improved defaults reach the operator; an active line for
  a key a later release removes (open PR #214 removes `hyst_up`/
  `hyst_down`) would stop the file loading; and `[server]`'s presence
  switches the servers on, so it has to start commented out anyway. Every
  key line carries an explanation, trailing or on the line above. The
  `[input]`, `[spot]`, `[detector]` and `[decode]` headers are active
  (empty tables are valid), so an uncommented key lands in the right
  table; `#[server]` and `#[[rbn_uplink]]` stay commented.
- **D10 — keys with no default show a marked example.** `N0CALL` for
  callsigns (it passes the syntax check but is not a valid US call),
  `<…>` for hosts, plausible numbers with the marker in the trailing
  comment. `bind_addr` shows the real default `"0.0.0.0"` with the
  exposure warning inline (2026-09-05 broad review, lens 1, finding #13;
  `2026-09-06-broad-review-decisions.md` D14). `check`'s placeholder guard
  closes the loop: an uncommented but unedited example fails check, naming
  the key.
- **D11 — the scaffold is a real file.** `crates/manta-cli/src/
  config_init.toml`, compiled in with `include_str!`: readable and
  diffable as TOML. Generating it was rejected: no config struct derives
  `Serialize`, `[detector]`'s millisecond values cannot be derived from
  hop counts exactly, and the per-key prose is hand-written anyway. Tests
  in `config_cmd.rs` pin it to the code instead: its key set equals the
  loader's own (`expected one of …`) list per table; as written it loads
  as the built-in defaults; every default-valued line, uncommented, leaves
  the typed config unchanged; the six server limits and the ppm fallback
  equal their code constants; every example loads when uncommented; and
  every key line is explained. A PR that adds, removes or re-defaults a key
  fails the build until the scaffold follows.
- **D12 — code layout.** `crates/manta-cli/src/config_cmd.rs` holds
  `check`, `init`, the summary, the notes, the two guards and their unit
  tests. `main.rs` gains only the subcommand, two dispatch arms and
  `CliOverrides::none()` (promoted from a test helper). End-to-end tests
  live in `crates/manta-cli/tests/config_command.rs`.

## Consequences

- An operator can validate a file, including the environment a service
  manager sets, without a receiver attached and without freeing the
  node's ports.
- The scaffold cannot silently go stale: a key change elsewhere fails
  `scaffold_lists_exactly_the_keys_the_loader_accepts` or
  `every_default_valued_line_is_the_built_in_default`. Open PRs #214
  (`hyst_frac`) and #128 (`operator_*` server keys) must update
  `config_init.toml` when they land.
- `check` is stricter than `run` in two places (duplicate ports,
  placeholders) and gentler in one (the dial guard is a note).

## Follow-ups (not in this change)

- `run` could also reject unedited placeholders.
- `--json` output for `check` (needs `Serialize` across the config types).
- Re-scope the scaffold's `bind_addr` lines when MAN-132 lands
  per-listener bind addresses. Done by MAN-132 (2026-10-08): the
  scaffold now has separate `bind_addr` and `metrics_bind_addr` blocks,
  and `check`'s notes and duplicate-port rule are per listener — see
  `docs/DECISIONS/2026-10-08-man132-metrics-loopback-bind.md`.
- Generate a shipped `manta.example.toml` from `manta config init --out -`
  if MAN-75's packaging goal is revived.

## References

- Ticket: MAN-76 (2026-09-05 broad review, lens 1, finding #13)
- Research and plan: the thoughts pool's
  `2026-10-07-MAN-76-operators-should-be-able-to-validate-a-config-file-without`
  documents
- `docs/DECISIONS/2026-10-06-man261-config-surface.md` (the config surface)
- `docs/DECISIONS/2026-09-06-broad-review-decisions.md` D14 (bind address)
- `ARCHITECTURE.md` §8, `README.md` "Run it as a node"
