# MAN-274: manta commits a Serena project config for agent symbol search

Date: 2026-10-17. Ticket: MAN-274, which replaces MAN-72 (PR #112, closed
by the 2026-10-05 audit triage; its branch survives as the tag
`archive/MAN-72-pr112` at `dce514d7be`).

## Problem

The catalyst-dev `codebase-analyzer`, `codebase-locator` and
`codebase-pattern-finder` agents prefer Serena's symbol tools and call
`activate_project` on the repository root. manta had no `.serena/`
directory, so there was no project for them to activate.

Activation does not fail without one. Reproduced on `main` at `e539c9b`
with Serena 2.0.0.dev0 (oraios/serena `1de556f`): `activate_project` on
the checkout returned `Created and activated a new project with name
'catalyst-runner-workspace-pGnzuX'`, `list_memories` returned `{}`, and
Serena wrote an untracked `.serena/` holding `project.yml`
(`ignored_paths: []`, `read_only: false`), `project.local.yml`,
`.gitignore` and `cache/`. The project took the checkout directory's name
and had no ignores, no memory and the editing tools enabled.

## Decision

Three files are committed under `.serena/`, and
`crates/manta-decode/tests/serena_project_config.rs` guards them:

- **`.serena/project.yml`** names the project `manta`, indexes Rust with
  rust-analyzer, and spells out all 25 `ProjectConfig` fields of Serena
  2.0 under their current names. A missing field, or a legacy key such as
  `languages:`, makes Serena re-save the file when it loads it and drop
  every comment. Serena 1.7 ignores the three keys it does not know
  (`agent_interface`, `excluded_apis`, `included_apis`).
  - `read_only: true` limits the project to navigation tools.
    `activation_command` stays empty, because Serena runs it as a shell
    command on every activation.
  - `agent_interface: tools` keeps the one-tool-per-operation interface
    (`find_symbol`, `find_referencing_symbols`, ...) that the agents call
    by name. Serena 2.0.1 makes the REPL interface the default for new
    installs. The project value applies only when Serena starts with
    `--project` on this repository; see the notes below.
  - `ignored_paths` adds what `.gitignore` does not reliably cover:
    `.serena/cache`, `/target`, `/.catalyst-cache`, `/.catalyst`,
    `/thoughts` and `/.worktrees`. `/.catalyst-cache` is the important
    one: some runners put `CARGO_HOME` there and hide it only through
    `.git/info/exclude`, which Serena never reads.
  - `language_servers` is `rust` only. The 25 Python files under
    `scripts/` stay visible to pattern search but get no language server:
    a second server would add a second startup that can fail, for tooling
    outside the decode code.
  - `initial_prompt` sends readers to CLAUDE.md's "Documents (read in this
    order)" section instead of copying the list, so the two cannot drift.
- **`.serena/.gitignore`** keeps `/cache` and `/project.local.yml` out of
  git. Serena writes the same file when it is missing; committing it means
  `git clean` cannot remove it.
- **`.serena/memories/codebase_map.md`** is the crate map
  `codebase-analyzer` reads first, with `read_memory("codebase_map")`.

The guard checks the exact key set and rejects legacy keys, pins
`project_name`, `agent_interface`, `read_only`,
`ignore_all_files_in_gitignore` and an empty `activation_command`, and
requires the five load-bearing ignores. It also checks that no ignore
pattern hides any `.rs` file under any workspace member, that the memory
names every member crate, and that `initial_prompt` points at a CLAUDE.md
heading that exists. CI has no paths filter, so the guard is regression
insurance; it is not what makes CI run on a `.serena/`-only change.

## Notes for agents and developers

1. **The tool surface comes from the global config.** On Serena 2.0.x,
   `agent_interface` in `~/.serena/serena_config.yml` fixes the MCP tools
   when the server starts. A fresh 2.0.1 install writes `REPL` there and
   exposes only `activate_project`, `initial_instructions` and
   `serena_repl`; the project's `tools` value does not change that after
   a mid-session `activate_project`. Started with `--project <manta root>`,
   the same install exposes nine read-only tools: `get_symbols_overview`,
   `find_symbol`, `find_referencing_symbols`, `find_implementations`,
   `find_declaration`, `get_diagnostics_for_file`, `read_memory`,
   `list_memories` and `initial_instructions`. To get the classic tools
   in a session that activates projects later, set
   `agent_interface: tools` in the global file.
2. **Serena 2.0 needs a session id.** Call `initial_instructions` first;
   it prints `Your Serena session id is <id>`, which `activate_project`
   takes as `session_id`. Serena 1.7 has no session id.
3. **A re-saved `project.yml` means a field is missing.** If
   `.serena/project.yml` shows as modified after a session, a newer Serena
   added a field. Add it to the file and to `FIELDS` in the guard.
4. **The memory name has no `.md`.** The file is
   `.serena/memories/codebase_map.md`; the memory is `codebase_map`.
5. **Indexing needs rust-analyzer.** `rust-toolchain.toml`'s `minimal`
   profile does not install it: run `rustup component add rust-analyzer`.
   Where the toolchain directory is root-owned, copy it first, following
   its symlinks: `cp -rL <rustup home> /tmp/rustup-home`, then run with
   `RUSTUP_HOME=/tmp/rustup-home`.
6. **Feature-gated code resolves poorly.** rust-analyzer builds default
   features, so `find_referencing_symbols` on `SoapySdrIqSource`
   (`manta-input/src/soapy.rs`, behind `soapy`) returns `{}` even though
   `manta-cli/src/main.rs` calls it. Grep for callers there.
7. **Reference lookups are slow.** `find_referencing_symbols` on
   `TrackDecoder` takes 13–28 s; the other probes return in under 3 s.

## PR #112 and its open threads

PR #112 landed the same three files, a guard test and a wiki page, built
against Serena 1.7.1's 22 fields. It cannot be reused unchanged. Serena
2.0.0 re-saved its `project.yml` on the first index pass (md5 `7295187…`
to `96c3abb…`), deleting the comments and appending the three missing
fields. Its reading-order guard also fails on current `main`, because
CLAUDE.md's list now starts with `docs/README.md`.

| Thread (P2) | State at close | Here |
|---|---|---|
| Preserve the mandated document reading order | resolved | `initial_prompt` points at the CLAUDE.md section instead of listing documents |
| Check source files as well as source directories | **open** | Fixed. The PR head checked `.rs` files only under `src/`, `tests/`, `benches/` and `examples/`. With `- "build.rs"` added to `ignored_paths` (hiding `crates/manta-cli/build.rs`) its guard still passed. This guard walks each member's whole directory, and the same edit fails it. |
| Strip valid YAML inline comments before comparisons | resolved | Same comment-aware reader, with its own tests |

The PR's wiki page is replaced by this record. A new wiki page would need
a hashed `wiki/manifest.json` entry, and the wiki points at records rather
than restating them.

## Evidence

Measured on `e539c9b` with the three files and the guard in place.

| Serena | `serena project index .` | `project.yml` after | Probes |
|---|---|---|---|
| 1.7.1.dev0 (`9f9db76`) | `rust=237` | unchanged | activation as `manta`, all lookups real |
| 2.0.0.dev0 (`1de556f`) | `rust=237` | unchanged | activation as `manta`, all lookups real |
| 2.0.1.dev0 (`d9e4c79`), `--project` | `rust=237` | unchanged | all lookups real, nine read-only tools |

237 is the 236 tracked `.rs` files plus the guard. On 2.0.0:

- `find_symbol("TrackDecoder")` returns `crates/manta-decode/src/decoder.rs`,
  body lines 98–180.
- `find_symbol("TrackManager")` returns `crates/manta-engine/src/track.rs`,
  body lines 1371–1453. Serena's line numbers are 0-based.
- `find_referencing_symbols` on `TrackDecoder` returns 63 references in 9
  files across `manta-decode`, `manta-engine` and `manta-testkit`,
  including their `tests/`, `benches/` and `examples/`.
- `list_memories` returns `{"memories": ["codebase_map"]}`.
- `get_current_config` lists navigation tools only, and `replace_content`
  fails with `Tool 'replace_content' is not active`.

## Not done here

These are outside manta, for the owner to file against the catalyst-dev
plugin:

- The agents call `activate_project` without the `session_id` that Serena
  2.0 requires.
- They list `search_for_pattern`, `find_file` and `list_dir`, which
  Serena's `claude-code` context does not expose.
- They assume the `tools` interface that a fresh Serena 2.0.1 install no
  longer defaults to.
- Agents dispatched in cloud phase containers get no `mcp__*` tools at all
  (`repos/catalyst-workspace/shared/friction/CAT-82.md` in the thoughts
  pool).
