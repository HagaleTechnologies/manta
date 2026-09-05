# MAN-36: exact Rust toolchain pin (`rust-toolchain.toml`)

Before this change, three independent signals decided which Rust compiler
built manta, none synchronized with either of the others:

1. `Cargo.toml`'s `rust-version = "1.85.0"` — an MSRV *floor*. Cargo refuses
   to build with a compiler older than this, but any compiler `>= 1.85.0`
   satisfies it identically; it cannot select or pin a specific release.
2. CI's `dtolnay/rust-toolchain@<sha> # stable` step (five occurrences:
   `.github/workflows/ci.yml`'s `test`/`test-soapy`/`test-hpsdr` jobs,
   `.github/workflows/release.yml`, `.github/workflows/release-publish.yml`).
   The Action's README states the installed Rust version is governed by the
   **@rev of the Action**, not by any `toolchain:` input default. manta
   SHA-pins the Action's own commit for supply-chain hygiene (protecting
   against the Action's code changing underneath the workflow), but that pin
   is on the Action's `stable` branch, which resolves the *live* Rust stable
   release at each run — the Rust version itself still floats.
3. Whatever `rustup` default toolchain happened to be active on a given
   contributor's machine — nothing in the repo constrained this at all.

A fourth, unnamed-until-now floating signal: `Dockerfile`'s builder stage
(`FROM rust:1-slim-bookworm`) is also a floating major-version tag.

Concretely observed during this ticket's implementation: the sandbox's
preinstalled toolchain was `rustc 1.98.0` while
`https://static.rust-lang.org/dist/channel-rust-stable.toml` — the same
manifest `dtolnay/rust-toolchain@stable` resolves against — served `1.98.1`
at the same moment. A local build and a CI run one patch release apart,
observed directly rather than hypothesized.

## Options considered

1. **`rust-toolchain.toml` pinning an exact release** (chosen). `rustup`'s
   `cargo`/`rustc` proxy binaries read this file from the working directory
   and its ancestors on *every* invocation, ahead of any other default or
   override, and auto-install the named toolchain (plus any declared
   `components`) on first use if it isn't already present. This is `rustup`'s
   own pre-existing mechanism — no new tool, no workflow edit.
2. **Add a `toolchain:` input to each of the five `dtolnay/rust-toolchain@…`
   steps.** Rejected: the Action's README requires moving to
   `dtolnay/rust-toolchain@master` to accept an explicit `toolchain:` input
   ("the default is to match the @rev"), which would weaken this repo's
   SHA-pinning convention for third-party Actions
   (`docs/superpowers/plans/2026-07-25-m2-soapysdr-input.md:19`). It would
   also duplicate the version string across five workflow steps — recreating
   the exact drift surface this ticket removes, just moved from "no pin" to
   "five pins that can independently go stale."
3. **A cross-language toolchain manager (e.g. `mise`).** Rejected per the
   ticket's own Outcome: this repo is Rust/cargo only today; a
   cross-language manager is worth revisiting only if a second ecosystem
   needs coordinating alongside Rust. `rustup`'s own pin file is simpler and
   more idiomatic for a single-ecosystem repo.

## Decision: `rust-toolchain.toml`, pinning whatever CI's floating step resolves to at pin time

`rust-toolchain.toml` (repo root):

```toml
[toolchain]
channel = "1.98.1"
profile = "minimal"
components = ["rustfmt", "clippy"]
```

- **`channel`** is always set to what
  `https://static.rust-lang.org/dist/channel-rust-stable.toml` reports at the
  moment of the pin/bump — the same manifest `dtolnay/rust-toolchain@stable`
  resolves against — recorded verbatim in the file's own comment. Never a
  guessed or aspirational version. This is the same methodology the sibling
  `widdershins` repo used for its analogous `.mise.toml` Node/npm pin (PR
  #318): derive the pin from what CI's own toolchain-install step *actually*
  resolves to, not from an arbitrary "latest."
- **`components = ["rustfmt", "clippy"]`** mirrors exactly what the CI
  toolchain-install steps request, so a fresh clone can run the identical
  `cargo fmt`/`cargo clippy` gate CI runs.
- **`profile = "minimal"`** avoids pulling `rust-docs` that no build step
  reads, on CI runners and in the Docker builder stage alike. A contributor
  who wants std sources in their IDE runs `rustup component add rust-src`
  once, locally.
- **Deliberately no `targets` key.** Every release-matrix leg in
  `release.yml` that doesn't use `cross` builds its own host triple, and host
  `rust-std` ships with every toolchain install regardless. Adding all five
  release targets would make every contributor and CI job download
  `rust-std`s nothing on that machine builds against.

Because `rustup` evaluates the directory-scoped override on every
`cargo`/`rustc` invocation regardless of what installed the *default*
toolchain, this single file is sufficient on its own for CI, a fresh local
clone, the Docker builder stage, and any other rustup-managed runner to
converge on the pinned compiler. **No edit to any of the five
`dtolnay/rust-toolchain@…` workflow steps was made or is required.**

### The `cross`-built release legs

`release.yml`'s two Linux release targets build via `cross` (pinned
`cargo install cross --version 0.2.5 --locked`) inside a cross-rs-maintained
Docker container, rather than directly on the GitHub-hosted runner. Verified
by reading `cross` 0.2.5's own source (no `rust-toolchain.toml` awareness
anywhere — `grep -rni rust-toolchain` over the crate returns nothing): `cross`
derives its container toolchain by running `rustc --print sysroot` in the
project directory (`src/rustc.rs:96-134`) and taking the sysroot's directory
basename. That `rustc` is the rustup proxy, so with the pin present it
resolves to `~/.rustup/toolchains/1.98.1-<host-triple>`, and `cross` installs
exactly that toolchain, plus the target's `rust-std`, into its container
(`src/lib.rs:433-473`). The pin reaches `cross`'s container transitively,
with no `Cross.toml` change. `release.yml`/`release-publish.yml` don't run on
PRs, so this was confirmed by source-level analysis in this session rather
than a live release run; the next tagged release is the first opportunity to
confirm this from real `cross build` logs (no `warn_host_version_mismatch`
per `cross` `src/lib.rs:581-603`). If a mismatch ever appears there, the
contained fix is a `cross`-leg-only `toolchain:` input — not a change to this
pin.

### The guard: `scripts/check-rust-toolchain-pin.sh`

A pin that silently stops being honored (someone sets `channel = "stable"`, a
step runs `cargo +stable`, a runner image ships a non-rustup `rustc` ahead of
`PATH`) degrades back to exactly the floating behavior this ticket removes,
with no symptom until two builds disagree — the same class of problem as
having no pin at all. `scripts/check-rust-toolchain-pin.sh` makes that a
failed required check instead: it asserts the pin file exists, names an exact
`MAJOR.MINOR.PATCH` channel (not `stable`/`beta`/`nightly`/a two-component
version), that the pinned channel is `>=` `Cargo.toml`'s `rust-version` MSRV
floor, and that the active `rustc --version` matches the pin exactly. It
needs no network and no `jq` (unlike
`scripts/check-dependabot-cargo-unlock.sh`, which needs both and is
deliberately *not* wired into required CI for that reason), so it *is* wired
into `.github/workflows/ci.yml`'s required `test` job, immediately after
`Swatinem/rust-cache` and before `cargo fmt --all --check` — early enough
that a mismatch is reported as itself, not as a confusing downstream
fmt/clippy diff. `test-soapy`/`test-hpsdr` share the same checkout and runner
images and are not separately instrumented, to keep the diff minimal.

Asserts on `rustc --version`, not `cargo --version`: Rust's release-channel
manifest lists Cargo under its own, different version series (e.g.
`[pkg.cargo] version = "0.99.0"` for Rust 1.98.1) — comparing `cargo
--version` against the pinned channel string would be asserting on a
coincidence, not the compiler identity the ticket's Gherkin scenario cares
about.

### Interaction with `Cargo.toml`'s MSRV

Unchanged and orthogonal. `Cargo.toml`'s `rust-version = "1.85.0"` remains
the separate, lower support floor it has been since M0
(`docs/DECISIONS/2026-07-11-m0-implementation-pins.md`); the guard enforces
pin `>=` floor, so the two can never silently contradict each other.

### Known accepted cost

One redundant toolchain download per CI job: the
`dtolnay/rust-toolchain@…#stable` step still installs whatever "stable"
floats to today, and then the first `cargo` invocation in the job
auto-installs the pinned version as a second download. Both land on the
identical final compiler, so this is CI bandwidth/time, not a divergence
risk. If CI minutes ever become a concern, the contained follow-up is a
single workflow-level `env:` value feeding a `toolchain:` input on all five
steps — one source of truth instead of five — at the cost of the
`@master`-vs-SHA-pin tradeoff named above.

## Bump procedure

Bumping the pin is manual by design — Dependabot has no ecosystem for
`rust-toolchain.toml`. To bump:

1. Fetch `https://static.rust-lang.org/dist/channel-rust-stable.toml` and
   read the `[pkg.rust] version` line.
2. Update `channel` in `rust-toolchain.toml` to that exact version, and
   update the file's own comment to record the new version string and the
   date it was read.
3. Open a PR touching only `rust-toolchain.toml`. CI's required `test` job
   (`scripts/check-rust-toolchain-pin.sh`, `cargo fmt --all --check`, `cargo
   clippy --workspace --all-targets -- -D warnings`, `cargo test
   --workspace`) is the acceptance gate — a new stable release's added lints
   or rustfmt style changes surface there and nowhere else.
4. Bump when a contributor needs a newer language/library feature or a
   security fix land, and otherwise at least once per release cycle, so no
   single bump ever has to absorb more than one cycle's worth of lint/format
   drift.

## What we're not doing

- Not editing any of the five `dtolnay/rust-toolchain@…` workflow steps —
  see Option 2 above.
- Not removing the `dtolnay/rust-toolchain` steps entirely. GitHub-hosted
  runners ship `rustup`, so the pin alone would technically be enough, but
  removing the explicit install step would make CI depend on an undocumented
  runner-image property instead of an auditable step.
- Not adding a `targets` key — see Decision section.
- Not changing `Cargo.toml`'s `rust-version` MSRV floor.
- Not hard-pinning `Dockerfile`'s `rust:1-slim-bookworm` base tag. The
  toolchain pin already forces the builder stage onto the pinned release
  regardless of what the base image ships (`COPY . .` carries the pin file
  into the image, and `.dockerignore` does not exclude it); hard-pinning the
  base tag itself is a separate image-hygiene decision, out of scope here.
- Not writing the guard as a Rust `#[test]`. It would run under `cargo test
  --workspace` in environments that legitimately have no `rustup` (a distro
  `rustc`, downstream packagers, the Pi4 runbook host in
  `docs/RUNBOOKS/m2-pi4-cpu-budget.md`) and would hard-fail there for no
  benefit. A shell script invoked only by CI (and on demand) keeps `cargo
  test` itself environment-agnostic.
- Not adding a `CONTRIBUTING.md` or README "build from source" section —
  out of scope for this ticket; this ADR plus the pin file's own comment
  header are the documentation this change owes.
- Not automating pin bumps (no Dependabot ecosystem exists for
  `rust-toolchain.toml`); the manual bump procedure above stands in for it.

## References

- Ticket: MAN-36
- `Cargo.toml:15-19` — `[workspace.package]`, `rust-version = "1.85.0"` (MSRV floor, unchanged)
- `.github/workflows/ci.yml` — the `test` job's new `scripts/check-rust-toolchain-pin.sh` step; the other four `dtolnay/rust-toolchain@…` steps (unchanged)
- `Dockerfile` — floating `rust:1-slim-bookworm` builder base; picks up the pin via `COPY . .`
- `Cross.toml` — per-target `pre-build` hooks; no Rust-version configuration, correctly so
- `scripts/check-dependabot-cargo-unlock.sh` — the shell-script conventions the new guard follows
- `docs/DECISIONS/2026-07-11-m0-implementation-pins.md` — the M0 ADR locking `rust-version = "1.85.0"`
- Sibling-repo pilot this ticket's Motivation cites: `widdershins` PR #318, `.mise.toml` (analogous Node/npm exact-pin fix, same "derive from what CI actually resolves" methodology)
- `cross` 0.2.5 source: `src/rustc.rs:96-134` (`sysroot()`/`get_sysroot()`), `src/lib.rs:433-473`, `:581-603`
