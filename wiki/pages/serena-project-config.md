# Why does manta commit a .serena/ directory, and what breaks if it changes?

`.serena/project.yml` is the activation contract that lets the catalyst-dev
`codebase-analyzer`/`codebase-locator`/`codebase-pattern-finder` subagents get
real symbol search (`find_symbol`, `find_referencing_symbols`,
`get_symbols_overview`) instead of silently falling back to grep (MAN-72).
Each of the following was learned by reproducing the failure live, not by
reading the config alone:

1. **Missing config does not error, it auto-generates.** Point
   `activate_project` at a repo with no `.serena/` and Serena does not fail —
   it silently writes a `project.yml` named after the *checkout directory*
   (not this repo's name), with `ignored_paths: []` and `read_only: false`,
   plus three other untracked files. Symptom: `find_symbol` over the project
   root stops returning inside its own 240 s tool budget rather than reporting
   an error.
2. **`ignore_all_files_in_gitignore` does not cover `.git/info/exclude`.**
   Serena's `GitignoreParser` reads only files literally named `.gitignore`.
   Some hosts relocate `CARGO_HOME` to `.catalyst-cache/cargo` inside the repo
   root and exclude it only via a machine-local `.git/info/exclude` entry —
   invisible to Serena. This is why `/.catalyst-cache` is an explicit
   `ignored_paths` entry: without it, an index pass walks ~10,000 vendored
   third-party `.rs` files instead of manta's own ~85.
3. **Every field must be spelled out, using current key names.** Serena
   migrates the legacy `languages:` key to `language_servers:`; a renamed *or
   missing* field makes it re-serialize the whole file on activation and
   delete every comment in it. `crates/manta-decode/tests/
   serena_project_config.rs` pins both the current key name and the full
   22-field list.
4. **`read_memory` takes `memory_name`, not a filename, and it has no `.md`
   suffix.** The file on disk is `.serena/memories/codebase_map.md`; the
   memory is addressed as `codebase_map`.
5. **The index pass needs a working `rust-analyzer`.** A plain rustup shim can
   report `Unknown binary 'rust-analyzer' in official toolchain` rather than
   running it; `rustup component add rust-analyzer` against a writable
   `RUSTUP_HOME` resolves it.
6. **rust-analyzer's cold-start warm-up is slow — this is normal.** A
   `did not signal readiness within 120s; proceeding anyway` warning during
   indexing, and a first tool call after cold activation taking over a
   minute, are not signs of a broken config.
7. **The guard test is not a CI trigger here.** manta's
   `.github/workflows/ci.yml` has no paths filter — every PR already runs the
   full `ubuntu-latest`/`macos-latest` test matrix regardless of which files
   changed. The guard test exists purely as regression insurance against a
   silent rename/tidy-up dropping this repo back into grep-fallback.
