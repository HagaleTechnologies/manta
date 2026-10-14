# 2026-10-13 — MAN-133: shell completions and a man page for the manta CLI

**Status:** Implemented (branch `MAN-133`). `manta completions <SHELL>` and
`manta man-pages --out <DIR>` are rendered by `crates/manta-cli/src/cli_docs.rs`;
the release workflows' `validate-tag` job and
`scripts/package-release.py --man-dir` ship the pages in every archive.

## Context

The ticket (found in the 2026-09-05 broad review, lens 1 #22 / O-14) asks
for two things operators expect from any CLI they already use:

1. `manta completions bash` (or `zsh`/`fish`) prints a script that, once
   sourced, tab-completes manta's commands and flags.
2. After installing from a release, `man manta` shows a page generated from
   the CLI's own help text.

Neither existed: the CLI is a single clap-derive tree (`struct Cli` in
`crates/manta-cli/src/main.rs`), and the workspace had no `clap_complete`
or `clap_mangen` dependency. Everything below was prototyped and measured
before the plan was written; the four facts that changed the research's
design are D3, D5, D11 and D7's artifact name.

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | Both features render from `Cli::command()` through `clap_complete` 4.6 (`aot` module) and `clap_mangen` 0.3. | No duplicated text, so the scripts and pages cannot drift from `--help`. The two crates are `manta-cli` `[dependencies]` (only that crate uses them). The crate-root re-exports of `clap_complete` are documented as deprecated aliases of `aot`, so the code imports from `aot`. |
| D2 | All five shells `clap_complete::aot::Shell` supports: bash, elvish, fish, powershell, zsh. | The ticket names bash/zsh/fish as examples. PowerShell matters for the Windows archive; elvish costs nothing. |
| D3 | The man-page writer is the public `manta man-pages --out <DIR>`, not a hidden command. | **Measured:** `clap_complete` 4.6.11 writes hidden subcommands into all five shells' scripts (a prototype `#[command(hide = true)]` command appeared in every script), so a hidden command would still be offered on every `manta <TAB>`. Public also serves source builds and the Docker image. `--out` follows `gen --out` and `config init --out`. |
| D4 | Nothing in the CLI may be hidden. A unit test walks `Cli::command()` and fails on any hidden subcommand or argument; another runs `Cli::command().debug_assert()`. | clap_complete publishes hidden items (D3), so `hide` is not a usable mechanism anywhere in this tree. `cli_docs` would need a filter first. Both tests are guards, written in Phase 1 so nothing can regress them. |
| D5 | `.TH` fields: date `""`, source `manta <VERSION>`, manual `User Commands`. | **Measured:** clap_mangen 0.3.3 (roff 1.1.1) quotes only `.TH` arguments containing a space, so the default empty date vanishes and the version string slides into the date field on every page (`mandoc -T lint`: "cannot parse date"). `""` is roff's explicit empty argument; a real date would need a build-time clock (non-reproducible pages) or a new `MANTA_*` rustc-env constant. The source is `manta 0.1.0`, not the full `VERSION_LINE`, because groff truncates long footers at 80 columns; the full identity stays in `manta.1`'s `VERSION` section. Subcommands carry no clap version, so the source is set for every page. |
| D6 | No page for clap's `help` subcommand: `man_pages()` sets `disable_help_subcommand(true)` on its own copy of the tree. | Same as `clap_mangen::generate_to`; keeps `SUBCOMMANDS` from listing a `manta-help(1)` that does not exist. Completions keep `help`, because `manta help run` works. |
| D7 | Release archives get the pages from one native rendering per workflow, at the end of the `validate-tag` job (after its tag checks), uploaded as artifact `man-pages`; each build leg downloads it and `scripts/package-release.py --man-dir` stages them under `man/man1/` (mode 0644). | Page text depends only on Cargo features, and every leg builds `--features hpsdr` (pinned by a test), so one rendering is correct for all five archives; the aarch64 leg's cross-built binary cannot run on its x86_64 runner. The artifact must not match `manta-*` (its name): **measured**, the `release` job downloads `pattern: manta-*` and publishes everything it downloads, so a `manta-man-pages` artifact would become a release asset. Cost: one more cold native build before the matrix (~5½ min on 32 cores here; GitHub-hosted runners slower — releases are owner-triggered). Not a job of its own: `scripts/tests/release-version.test.sh` (MAN-65) and `scripts/tests/release-ancestry.test.sh` (MAN-244) require `build` to need exactly `validate-tag`, so a separate `man-pages` job that `build` needs failed them; the build runs with `validate-tag`'s persisted read-only checkout token, as every build leg's does. Rejected: each leg running its own binary (aarch64 cannot), committing generated pages (20 files churning on every help-text edit; the git SHA in `VERSION` cannot be committed), extending `ASSETS` with download-only paths (breaks local packaging and the `KIT`-based tests). |
| D8 | `--man-dir` is required and fails closed: a missing `manta.1` is a `missing required input` (exit 1, no archive), like a missing licence (MAN-268). Every regular `*.1` file directly in the dir is staged; other files ignored. | Archives cannot silently ship without pages once the README documents `man/man1`. `ASSETS` stays a list of checked-in files. |
| D9 | Archives do not ship completion-script files. | Every binary prints its own script (`manta completions <shell>`); packagers expect exactly that (Homebrew's `generate_completions_from_executable`). Files would add a second generated artifact to the release jobs and packaging tests. |
| D10 | The Docker image does not change. | `docker run … completions bash` and `docker run … man-pages --out /out` already work through the MAN-81 argument pass-through; the image has no `man` or interactive shell to attach either to. |
| D11 | bash and zsh instructions use `eval "$(manta completions <shell>)"`, not `source <(…)`. zsh needs `compinit` first. | **Measured:** `source <(…)` is a silent no-op in macOS's bash 3.2. macOS's default `~/.zshrc` does not run `compinit`. fish uses a file in `~/.config/fish/completions/`. The generated bash script branches on `BASH_VERSINFO` itself, so it loads on bash 3.2. |
| D12 | Known upstream gaps are documented, not fixed: `--engine` values do not complete (hand-written `value_parser`, so shells fall back to file names); fish does not complete `manta completions <TAB>` shell names; clap_mangen names the `after_help` section `EXTRA`; the synopsis lists option names without values; the root page's `EXTRA` wraps one example line at `MANWIDTH=80`; `br`/`sp` mandoc style warnings. | All come from the upstream generators. Fixing them means re-implementing clap_mangen's private section logic or changing `--help` text that is not this ticket's to change. Follow-ups file what is worth doing. |
| D13 | Docs: a README "Tab completion and man pages" subsection under `## Installation` (README ships in every archive and holds the install steps); `docs/RUNBOOKS/release.md` names the job and the archive contents; a CHANGELOG `### Added` entry; this record; pointers from `docs/README.md`, `CLAUDE.md` and `wiki/INDEX.md`. `packaging/README.md` deliberately does not change (man pages are not part of the service kit) and the MAN-270 record is not rewritten (records are never rewritten; this record states the two commands' streams). | Where operators already look. |

## Streams (extends MAN-270 by reference)

MAN-270's table (`docs/DECISIONS/2026-10-10-man270-cli-output-style.md`) says
commands that write files leave stdout empty and print the confirmation on
stderr, and errors are one `error:` line. The two new commands follow it:

| Command | stdout | stderr | Errors |
|---|---|---|---|
| `manta completions <SHELL>` | the completion script, byte-identical across runs | empty | clap usage errors, exit 2 |
| `manta man-pages --out <DIR>` | empty | `wrote <n> man pages to <dir>` | `error: <path> is not a directory`, exit 1 |

## What the prototype measured

- Real bash 5.2 (COMPREPLY probe), zsh 5.9 (zpty-driven interactive) and
  fish 3.7 completed every command path's subcommands and flags from the
  generated scripts: 0 mismatches over the 20 command paths; `listen`
  offered; the hidden `--server-config` alias not offered; `--log-level`
  values complete as exactly the six levels.
- `man manta` displayed by man-db 2.12 from pages unpacked out of a
  prototype archive; `mandoc -T lint -W error` exit 0 and `groff -man -ww
  -z` silent on all 20 pages; every flag in every command's `--help`
  appears in that command's page (20/20).
- Lockfile delta exactly `clap_complete 4.6.11`, `clap_mangen 0.3.3`,
  `roff 1.1.1`; `clap` stays 4.6.7. Release binary +408,576 bytes (+2.7%).
- Determinism: each shell's script and the pages are byte-identical across
  runs.
- Not verified here: PowerShell and elvish scripts (no interpreter;
  generated only), macOS bash 3.2/mandoc `man`, the GitHub Actions jobs
  themselves (owner-triggered; see the runbook), a `cargo audit`.

## Follow-ups

1. `--engine` value completion via a possible-values-aware parser shared by
   `decode`/`oracle`/`run`/`soak` (and `bench sensitivity --engine`); it
   changes those commands' `--help` and error wording, so it needs its own
   review.
2. A Homebrew tap (lens-1 #27) can use
   `generate_completions_from_executable(bin/"manta", "completions")` and
   `man1.install Dir["man/man1/*.1"]`.
3. Optional man-page polish: an `EXAMPLES` heading instead of `EXTRA`,
   `SEE ALSO manta(1)` on subcommand pages, an example block that does not
   wrap at 80 columns.
