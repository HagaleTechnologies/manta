# MAN-519: every text file checks out with LF; the vendored cty.dat and master.scp keep upstream's CRLF

Date: 2026-11-10. Base: `acbc00f0745e2b1146f7fecf1745abb9ac3eeb6b`.

Git for Windows defaults to `core.autocrlf=true`, so the `windows-latest`
runners checked out every text file with CRLF unless `.gitattributes`
named it. Before this change `.gitattributes` named three paths: the
`rbn_grade` and `skimsrv_pair` fixture trees and `patterns.lst`. A
`core.autocrlf=true` clone of the base wrote 906 of the 919 LF-stored
files as CRLF. In that clone `bash scripts/check-rust-toolchain-pin.sh`
fails with `set: pipefail: invalid option name` and exit 2.

## Decisions

1. **One repo-wide rule, first in the file.** `.gitattributes` now starts
   with `* text=auto eol=lf`. Every file git detects as text checks out
   with LF on every platform, and the attribute overrides
   `core.autocrlf`. The rule must stay first because the last matching
   line wins: the explicit rules below it still apply to their paths.
   The three existing rules stay as they were. A new byte-exact fixture
   or template no longer needs its own line.

2. **`cty.dat` and `master.scp` stay CRLF, pinned `-text`.** Both are
   vendored upstream downloads, stored with upstream's CRLF bytes, and
   `crates/manta-spot/data/SOURCES.md` says a refresh replaces the file
   with the download. With the global rule alone, `git add --renormalize .`
   rewrites every line of both: 3 files changed, 51,645 insertions and
   51,634 deletions in a scratch clone. With the two `-text` lines it
   stages only `.gitattributes`. `-text` means no conversion on any
   platform, so a Windows checkout writes the same bytes as Linux, which
   is what the ticket asks for. `-text` rather than `binary` keeps text
   diffs when a list is refreshed. Both parsers already accept either
   line ending (`cty.rs` tests a CRLF copy). The cost: the ticket title's
   "every text file ... LF" holds for every text file except these two.

3. **`*.der` is `binary` too.** The ticket named `*.wav`, `*.cf32`,
   `*.ci16` and `*.png`. None of the first three is tracked today (the
   audio corpus is fetched by `scripts/corpus.py`); the rules are there
   for the day one is. The four TLS fixtures under
   `crates/manta-cli/tests/fixtures/list_tls/` are the only other tracked
   binaries, embedded with `include_bytes!` in `list_http.rs`. Until now
   only git's NUL-byte heuristic kept them unconverted.

4. **No new guard test.** Under the global rule, `git add` stores a new
   CRLF text file as LF, so a CRLF fixture can no longer land by
   accident. The only way back is an edit to `.gitattributes`, which
   `.github/CODEOWNERS` routes to the owner; `scripts/tests/test_ci_gates.py`
   pins that ownership line, not the file's rules. The existing `\r\n`
   normalisation in tests stays; it is correct on LF input, and removing
   it is separate cleanup.

## Effect on Windows builds

Windows-built artifacts that copy or embed checked-out text now carry LF
instead of CRLF. Examples: the Windows release zip's `README.md`, licence
files and `manta.example.toml` (the `ASSETS` list in
`scripts/package-release.py`), and the `manta config init` scaffold
(`include_str!("config_init.toml")` in `crates/manta-cli/src/config_cmd.rs`).
These bytes now match the Linux and macOS builds, in line with the
deterministic-output constraint. No test asserts CRLF from a checked-out
file; the `\r\n` literals in `crates` and `scripts` are HTTP and telnet
wire bytes. On Linux and macOS no checked-out byte changes.

A Windows clone made before this change keeps its CRLF working files
after a pull: git rewrites a file only when its blob changes, and
`git status` stays clean. CI is unaffected, since every run is a fresh
clone. To refresh such a clone, commit or stash local work, then run
`git rm --cached -r -q . && git reset --hard`. Tried on a
`core.autocrlf=true` clone of the base moved to this change: before the
refresh 904 files were still CRLF and `check-rust-toolchain-pin.sh`
failed; after it only `cty.dat` and `master.scp` were CRLF and the
script passed.

## History

Each earlier incident was fixed by adding one more path:

- MAN-301 (#271): CRLF fixtures turned `test (windows-latest)` red on
  `main` until `f67a75d` pinned the `rbn_grade` fixtures to LF
  (`2026-10-13-man156-ci-regression-gates.md`).
- MAN-95 (#296): `patterns.lst` checked out CRLF and failed on Windows;
  #302 pinned it (`2026-10-15-man371-windows-required-check.md`).
- MAN-97: the `skimsrv_pair` fixtures were pinned when they arrived.
- MAN-30: a CRLF fixture kept `main` red for 22 hours and 18 runs on
  2026-11-06, fixed by hand in #530.

The repo-wide rule ends that pattern.

## Verification

- `git check-attr text eol -- Cargo.lock crates/manta-spot/data/cty.dat
  crates/manta-cli/tests/fixtures/list_tls/ca.der` reports `text: auto`,
  `text: unset` and `text: unset`, each with `eol: lf`.
- `git ls-files --eol` lists exactly `cty.dat` and `master.scp` as
  `i/crlf`; `git add --renormalize .` stages nothing beyond
  `.gitattributes`.
- A `git clone -c core.autocrlf=true` of the branch writes CRLF only for
  those two files, and `scripts/check-rust-toolchain-pin.sh` passes in it.
- `test (windows-latest)` in `.github/workflows/ci-full.yml` is the
  standing proof.
