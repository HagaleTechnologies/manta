# CLI output style guide (MAN-130)

## Decision

`manta`'s output must read like a finished product, never a raw Rust Debug
rendering. This is the normative style every command follows; `manta-cli`'s
`fmt` module (`crates/manta-cli/src/fmt.rs`) is the one place a frequency,
SNR, WPM, or confidence value is turned into text, so the same value never
reads two different ways in two commands.

| Value | Rendering | Example |
|---|---|---|
| Frequency | kHz, one decimal | `14000.7 kHz` |
| SNR | whole dB | `18 dB` |
| Speed | whole WPM | `17 WPM` |
| Speed, absent | the word, never `None`/`Some(..)` | `unknown` |
| Confidence | two decimals | `conf 0.81` |
| Spot context | human label, never a Rust variant | `CQ` / `DE` / `BEACON` / `unknown` |
| Booleans | `yes` / `no`, never `true`/`false` in prose | `panicked: no` |
| Errors | `error: <what failed>: <cause>` on one line, stderr | `error: open WAV ./nope.wav: No such file or directory (os error 2)` |
| Hints | optional `hint: <suggestion>` line, stderr, after the error | `hint: --source takes a 48 kHz mono audio WAV; use \`manta decode <file>\` for an IQ WAV` |

## stdout / stderr split

stdout carries the command's product; stderr carries everything else
(summaries, live monitors, `tracing` logs, errors):

| Command | stdout | stderr |
|---|---|---|
| `decode` | decoded text (`--json`: the full report) | one-line summary |
| `listen` | spot lines (`--json`: JSON Lines of spots/events) | live per-character monitor (text mode only) |
| `soak` | the report (text or `--json`) | nothing on success |

## Errors and hints

Every propagated error renders as one `error: <what failed>: <cause>` line
on stderr — `fmt::render_error` flattens an `anyhow::Error`'s cause chain
instead of anyhow's own multi-line `Caused by:` block. An optional `hint:`
line follows when a hint was attached via `.context(fmt::Hint("..."))`;
`fmt::Hint`'s `Display` is empty (so `.context()` doesn't pollute the error
line) and is retrieved with `anyhow::Error::downcast_ref`, not
`chain().find_map(..)` — each `chain()` link for a `.context(C)` layer is
anyhow's internal `ContextError<C, E>` wrapper, not `C` itself, so
downcasting the `&dyn Error` links never matches; `downcast_ref` does.

Clap's own usage errors (missing/bad argument) already print
`error: ...` and are unaffected by this: `Cli::parse()` exits the process
itself (code 2) before `manta-cli`'s `run()` ever returns.

## Exceptions

- **`SpotMessage.frequency` (the `:7301` wire contract) stays full-precision
  Hz**, not kHz. `docs/SPEC-decode-core.md` §1.4 mandates 0.1 kHz rounding
  for the telnet output and full Hz precision for the JSON stream — a
  distinct, pre-existing, deliberate design point this guide does not
  change.
- **`SpotType::rbn_flag()`'s blank `Unknown`** (`""`, not `"unknown"`) is a
  wire behaviour of the RBN cluster line's context column
  (`manta-server::rbn::format_line`) and must not change; `SpotType`'s
  `Display` impl is the human-readable one this guide governs.
- **`SpotType`'s JSON rendering follows the human label, not the Rust
  variant.** `#[serde(rename_all = "UPPERCASE")]` makes `manta decode
  --json` / `manta listen --json` emit `"spot_type":"CQ"` / `"UNKNOWN"`
  where they previously emitted `"Cq"` / `"Unknown"`, so the text and JSON
  modes agree on one spelling. This is safe to change here because
  `SpotType` reaches no contracted wire: `SpotMessage` (the `:7301`
  ecosystem schema) carries no `spot_type` field at all, and the enum
  derives `Serialize` but not `Deserialize`, so nothing in the workspace
  parses it back. Pinned by
  `context.rs::spot_type_serializes_as_the_human_label`.
- **`manta-soak-harness` (`man19-soak`)** is a separate, non-operator-facing
  CI binary, out of scope for this guide.

## Why

Three raw Rust Debug renderings reached the terminal in the shipped
`manta` binary (`wpm: Some(17.647058)`, `SoakReport { ... }`, a bare
`(Cq)` enum variant), and the numeric style around them was inconsistent
with the one surface that already got it right
(`manta-server::rbn::format_line`). Filed as MAN-130 from the 2026-09-05
five-lens broad review (lens 5 hit-list #17, lens 1 hit-list #24), which
also proposed the style table and stdout/stderr split this guide commits
to.

## Supersedes

The 2026-09-05 lens-5 review's "Output style guide (proposal)" section
(`thoughts/shared/reports/2026-09-05-manta-review-lens-5-docs-polish.md`)
— this document is that proposal, committed.
