# 2026-10-10 — MAN-83: build identity in `--version` and `decoderVersion`, and when decoder output may change

**Status:** Implemented (branch `MAN-83`). Records the design decisions made
while implementing MAN-83's two Gherkin scenarios: `manta --version` names
the exact build, and every JSON spot's `decoderVersion` reflects the real
build rather than a constant. It also records the decoder-output versioning
rule the ticket's technical notes ask for, which `CHANGELOG.md` summarises.

## Context

Before this change `manta --version` printed `manta 0.1.0` for every build,
whatever its commit or compiled-in features, and every JSON spot carried
`"decoderVersion":"manta-0.1.0"`. A support request could not tell which
build produced a report. The 2026-09-05 broad review found this from three
directions (hit-list R-13; lens 1 #21, lens 2 #16, lens 5 #9/#21).

MAN-128 (`2026-10-05-man128-node-health-metrics.md`, D5) had already added
`crates/manta-cli/build.rs`, which embeds the commit as `MANTA_GIT_SHA`
(override, else `git rev-parse --short=12 HEAD`, else `unknown`), and the
`manta_build_info{version,git_sha,features}` gauge on `/metrics`. Neither
`--version` nor `decoderVersion` read it.

## Decisions

- **D1 — extend MAN-128's `build.rs`; add no crate.** `vergen`, `built` and
  `shadow-rs` each need a `[build-dependencies]` entry in the CODEOWNERS-gated
  `crates/manta-cli/Cargo.toml` and `Cargo.lock`, and none adds anything over
  the existing hand-rolled script. The feature list is about 15 lines over
  `CARGO_CFG_FEATURE`.
- **D2 — one source of truth, `crates/manta-cli/src/build_info.rs`.** It holds
  `VERSION`, `GIT_SHA`, `FEATURES`, `VERSION_LINE` and `DECODER_VERSION`, all
  compile-time `concat!(env!(...))` constants. `--version`,
  `daemon_build_info()` (the `manta_build_info` gauge) and the JSON stream's
  `decoderVersion` all read it, so they cannot drift. The feature list comes
  from `build.rs` (generic over `CARGO_CFG_FEATURE`, sorted, `default`
  excluded, `none` when empty) rather than a hand-kept `cfg!` list, so a new
  feature appears without a code change. It reads `CARGO_CFG_FEATURE`, not
  `CARGO_FEATURE_*`: Cargo passes an inherited `CARGO_FEATURE_SOAPY=1`
  through to the build script, which would list a backend that was never
  compiled, while it always sets `CARGO_CFG_FEATURE` itself (empty when no
  feature is on). A unit test cross-checks it against
  `cfg!` for the features that exist.
- **D3 — no build date, no dirty flag.** A date makes two builds of one commit
  differ, and the ticket does not ask for one (R-13 did; the ticket narrowed
  it). A correct dirty flag needs the build script to rerun on every source
  edit, and a stale flag misleads more than a missing one. Release builds
  come from clean CI checkouts.
- **D4 — `decoderVersion` = `manta-<version>+<git field>`.** The commit is
  SemVer 2.0.0 build metadata, which carries no ordering meaning: the part
  before `+` is the release, and the whole string is the binary. Features
  stay out, because they choose input drivers (`hpsdr`, `soapy`) and never
  change decoding. This supersedes MAN-128's inline rationale (see
  "Supersedes" below), neither of whose two reasons holds:
  - *Determinism.* `docs/SPEC-decode-core.md` §6 item 4 requires
    byte-identical output from like-for-like builds, and CI's rule is "same
    binary + same IQ file". The commit is a compile-time constant, so one
    binary always emits one value. The outputs CI hashes (`manta decode
    --json`, `manta run --json`) carry no version string at all.
  - *Wire contract.* `decoderVersion` is an optional string in a dispensa
    contract still marked *Proposed*, and cqdx does not ingest it yet. The
    new value is still a string.
- **D5 — validate and normalise the `MANTA_GIT_SHA` override in `build.rs`.**
  The override must be SemVer build metadata: dot-separated, non-empty
  identifiers of `[0-9A-Za-z-]`, at most 64 characters. Otherwise `build.rs`
  emits a `cargo:warning` (value `{:?}`-escaped) and falls through to `git`,
  keeping MAN-128's "never fails the build" rule. This keeps `decoderVersion`
  a valid SemVer string and stops a newline in the override from injecting a
  second `cargo:` directive. A full git object name (exactly 40 or 64 hex
  digits), such as CI's `github.sha`, is cut to 12 and lowercased, so a
  later Docker build-arg build and a native build of the same commit report
  the same value. Any other override is kept as given: a looser "all-hex
  and longer than 12" test would also clip a numeric build ID such as
  `20261010123045`. A valid override also sets the `manta_git_sha_override` cfg,
  which tests use to skip their independent check against the checkout's
  HEAD.
- **D6 — the decoder-output rule.** See the next section; it is normative
  here and summarised in `CHANGELOG.md` and `docs/RUNBOOKS/release.md`.
- **D7 — the new compile-time variable is `MANTA_FEATURES`,** next to
  `MANTA_GIT_SHA`. `build.rs` never reads it from the environment, but it
  is still on `config.rs`'s `ENV_IGNORED`, beside `MANTA_GIT_SHA`: Cargo
  exports every build-script `rustc-env` value into the environment of the
  processes `cargo run` and `cargo test` start, and MAN-261's loader rejects
  unknown `MANTA_*` names. Measured while implementing:
  `spec_input_center_freq_hz_is_a_live_config_key` failed with
  `unrecognized environment variable MANTA_FEATURES` until it was added. The
  same export is why tests read the `manta_git_sha_override` cfg (D5)
  rather than their own `MANTA_GIT_SHA`, which `cargo test` always sets.
  `every_build_rs_env_name_is_ignored` now covers both the names `build.rs`
  reads and the names it exports.
- **D8 — no dispensa ADR before landing.** The value change stays within the
  field's type. The proposed schema `description` below is ready to lift into
  ADR-0011 or `spots.v1.schema.json`, following MAN-102's `snrRefHz`
  precedent. Filing it in dispensa is a follow-up.
- **D9 — no automated "decoder output changed ⇒ MINOR bump" check.** Golden
  vectors assert decoded text, not bytes keyed by version, and a
  byte-hash-per-version gate would fail on every legitimate decoder PR
  between releases. The CHANGELOG section plus the release-runbook step is
  the control.
- **D10 — test the wiring end to end.** `crates/manta-cli/tests/build_identity.rs`
  runs the real binary's `--version` and `-V`, checks the commit against
  `git rev-parse --short=12 HEAD` independently of `build.rs`, and runs a
  real `manta run` over the synthetic V1 recording, reading the first spot
  off the JSON stream's wire. It uses the full 120 s V1, not the 30 s
  `short_v1()`: at EOF the daemon signals shutdown, and the JSON stream drops
  a client still inside its 500 ms wall-clock WebSocket-detection peek, so a
  replay that ends under a second after the banner can lose the spot on a
  fast runner.
- **D11 — phase order.** Constants and `--version` first, `decoderVersion`
  second (it reuses the constant and the test file), documentation last.

## Decoder-output rule

1. **Decoder output** is the set of spots manta produces from a given input
   file and configuration: which calls are spotted, when, and at what
   frequency, SNR, speed, spot type and confidence. That is what
   `manta decode --json` and the spot lines of `manta run --json` record.
   `decoderVersion` is build identity, not decoder output.
2. A change that alters decoder output adds an entry under
   `### Decoder output` in `CHANGELOG.md`'s `## [Unreleased]`, in the same
   PR.
3. At release time, a `### Decoder output` section in Unreleased means the
   new version bumps at least the MINOR version over the previous release. A
   PATCH release never changes decoder output.
4. The rule applies to the workspace version (`[workspace.package] version`),
   which `--version` and `decoderVersion` carry and which the release tag
   must equal.
5. Between releases, `main` may change decoder output on any commit. For an
   untagged build only the full `decoderVersion` (with its `+<commit>`)
   identifies the build.
6. Enforcement is process, not CI (D9): the CHANGELOG entry and
   `docs/RUNBOOKS/release.md`'s "Cutting a release" step.

## Formats

- `manta --version` and `manta -V` print one line,
  `manta <version> (git <sha>; features: <list>)`, for example
  `manta 0.1.0 (git 1a2b3c4d5e6f; features: hpsdr)`. The first two tokens
  stay `manta <semver>`. `<sha>` is the 12-hex commit, a validated override,
  or `unknown`. `build.rs` takes the commit from git only when git's top
  level is the workspace root, so a source tree with no `.git` of its own,
  built beneath some other repository, reports `unknown` rather than that
  repository's HEAD. Its git calls also clear an inherited `GIT_DIR`,
  `GIT_WORK_TREE` and `GIT_COMMON_DIR`, which could otherwise point that
  check and the HEAD lookup at another repository. `<list>` is the sorted, comma-separated compiled-in Cargo
  features, or `none`, and is the same string as `manta_build_info`'s
  `features` label.
- JSON spot `decoderVersion`: `manta-<version>+<sha>`, for example
  `manta-0.1.0+1a2b3c4d5e6f`.
- `manta_build_info{version,git_sha,features}` is unchanged in form and
  value.

## Supersedes

MAN-128's inline comment at the `decoder_version` assignment in
`crates/manta-cli/src/main.rs`:

> MAN-128: stays SHA-free and feature-free on purpose -- this is the JSON
> spot wire contract (ARCHITECTURE §7) and a byte-identical-replay input
> (AGENTS.md's "file input -> byte-identical spot logs" hard requirement),
> neither of which may vary with the commit or build flags a given binary
> happens to carry. `manta_build_info` above is the right place for that
> information instead.

That rationale existed only as this comment. D4 gives the reasons it no
longer holds; the comment is replaced by one pointing here.

## Proposed spots.v1 fragment

For dispensa ADR-0011 or `contracts/spots/spots.v1.schema.json`:

```json
"decoderVersion": {
  "type": "string",
  "description": "Producer build identity: `<producer>-<semver>[+<build>]` (SemVer 2.0.0, optional build metadata). manta emits `manta-<crate version>+<12-hex git commit>`, or `+unknown` when built without git metadata. Compare the part before `+` to tell decoder releases apart; the build metadata identifies the exact binary."
}
```

## Deferred (follow-up tickets, no Linear access from this environment)

1. Docker and GHCR identity: add `ARG MANTA_GIT_SHA` to the `Dockerfile` and
   set `build-args: MANTA_GIT_SHA=${{ github.sha }}` in `release.yml` and
   `release-publish.yml` (all CODEOWNERS-gated; MAN-128 deferred item 2).
   Until then the image's `--version` and `decoderVersion` say `unknown`.
2. Startup banner, pipeline-ready line and `/status` / `manta status`: carry
   the commit and features (`manta-server`'s `StartupInfo` and `StatusDoc`).
3. Release notes from `CHANGELOG.md` instead of `generate_release_notes:
   true` (owner-gated `.github/`; broad review lens 5 #9).
4. Ship `CHANGELOG.md` in release archives (`scripts/package-release.py`,
   owner-gated).
5. dispensa: lift the proposed `decoderVersion` description above into
   ADR-0011 or `spots.v1`.
6. `CLAUDE.md` / `AGENTS.md` (owner-gated): one line pointing contributors at
   the CHANGELOG decoder-output rule.

## References

- Ticket: MAN-83. Source: 2026-09-05 broad review, hit-list R-13, lens 1
  #21, lens 2 #16, lens 5 #9/#21.
- Research: `thoughts/shared/research/2026-10-10-MAN-83-*.md`
- Plan: `thoughts/shared/plans/2026-10-10-MAN-83-*.md`
- `crates/manta-cli/build.rs`, `crates/manta-cli/src/build_info.rs`,
  `crates/manta-cli/tests/build_identity.rs`
- `docs/DECISIONS/2026-10-05-man128-node-health-metrics.md` (D5; Deferred
  item 2)
- `docs/SPEC-decode-core.md` §6 (determinism), `ARCHITECTURE.md` §7,
  `CHANGELOG.md`, `docs/RUNBOOKS/release.md`
