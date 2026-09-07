---
id: config-surface
title: How does manta's TOML config file work, and what overrides what?
kind: interface
status: current
maintainer: agent
sources:
  - docs/SPEC-decode-core.md#9-configuration
  - docs/DECISIONS/2026-09-06-man74-config-surface.md
verified:
  commit: a36c958
  date: 2026-09-07
links:
  - determinism
---
`manta run --config manta.toml` (alias: `listen`) reads one strict TOML file, `crates/manta-cli/src/config.rs`'s `ConfigFile` — six tables: `[server]`, `[[rbn_uplink]]`, `[input]`, `[spot]`, `[detector]`, `[decode]`. Unknown top-level tables and unknown keys inside a modeled table are both hard parse errors naming the offending key — there is no silent-ignore path left for any of the six. `manta decode`/`manta gen` never touch this loader at all, on purpose (see [[determinism]]): the byte-identical-decode contract must depend only on the input file, never on an ambient config file or environment variable.

## Pointers

- The full key table per `[detector]`/`[decode]`/`[input]`/`[spot]`, including which keys are compile-time constants the loader deliberately rejects rather than silently ignores: SPEC §9.
- Precedence (`CLI flag > MANTA_<TABLE>_<KEY> env var > this file > built-in default`), why the env tier is a TOML-document overlay rather than a second validation path, why `[input]` is an internally-tagged enum instead of `#[serde(flatten)]`, and why `[server]` is optional (servers start iff it's present): `docs/DECISIONS/2026-09-06-man74-config-surface.md`, Decisions 1–8.
- Why the CLI's `open_source`/`open_source_spec` split (one path for CLI flags, one for `[input]`) is still two parallel implementations rather than one collapsed function, and which shared `[input]` keys get suppressed when a CLI source flag overrides the table wholesale: same doc, Decision 9.

## Gotcha

`[input].freq_correction_ppm` and `[input].dial_freq_hz` both describe the *specific source* the `[input]` table names — when a CLI source-selection flag (`--kiwi-host`/`--soapy-*`/`--hpsdr-*`) overrides `[input]` with a different, RF-aware source, both keys are suppressed together (`CliOverrides::suppress_file_input_shared_keys`), not just one. Before this was unified, a WAV recording's stale calibration could silently apply to a live receiver the operator switched to on the command line.
