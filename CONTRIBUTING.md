# Contributing to manta

Start with an existing issue or open a GitHub issue describing the problem
and the outcome you want. Search issues and pull requests first so you can
coordinate with anyone already working on it. Bug reports, hardware
compatibility reports and feature requests each have a template.

You do not need access to our ticket tracker to contribute. If there is a
MAN ticket, link its full URL in your pull request. Otherwise link a public
GitHub issue; a maintainer will associate the MAN ticket before merge.
Report vulnerabilities through [SECURITY.md](SECURITY.md).

## Build and test

Install Git and Rust through rustup. Run commands from the repository root.
[rust-toolchain.toml](rust-toolchain.toml) selects the exact compiler and
the rustfmt and Clippy components. Git must be on PATH because Cargo fetches
the pinned coppa dependency through the Git executable.

On Debian, Ubuntu or Raspberry Pi OS, install the Linux build prerequisites:

```sh
sudo apt install build-essential libasound2-dev pkg-config
```

Other Linux distributions need their equivalent C build tools, ALSA
development headers and pkg-config packages. ALSA is needed even for
file-only use. On macOS, install the Xcode command line tools. On Windows,
use the MSVC target and Visual Studio Build Tools; see the
[Windows setup guide](wiki/pages/windows-build-setup.md). Run Bash scripts
in Git Bash on Windows.

```sh
git clone https://github.com/HagaleTechnologies/manta
cd manta
bash scripts/check-rust-toolchain-pin.sh
cargo build --locked -p manta-cli
```

The debug executable is `target/debug/manta`, or `target/debug/manta.exe`
on Windows. The default build does not include HPSDR or SoapySDR flags.

Run the core checks before requesting review:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

These mirror the core Rust checks in
[CI](.github/workflows/ci-full.yml). CI also runs release, packaging and
workflow checks and tests other platforms. Consult the workflow for those
commands when changing the corresponding files. Keep Cargo.lock committed;
do not drop `--locked` from verification to hide dependency drift. If you
intentionally change dependencies, regenerate and review the lockfile.

For changes affecting HPSDR, also run:

```sh
cargo clippy -p manta-input -p manta-cli --all-targets --features hpsdr --locked -- -D warnings
cargo test -p manta-input -p manta-cli --features hpsdr --locked
```

HPSDR needs no additional native library. To build an executable with its
flags, add `--features hpsdr` to the build command.

For SoapySDR changes, first install the native SoapySDR development library
and the driver module for your receiver. On Debian or Ubuntu the library
package is `libsoapysdr-dev`; on macOS use `brew install soapysdr`. The
Windows setup guide describes its separate setup and current CI gaps.
Then run:

```sh
cargo clippy -p manta-input -p manta-cli --all-targets --features soapy --locked -- -D warnings
cargo test -p manta-input -p manta-cli --features soapy --locked
```

Hardware-free tests do not establish real receiver compatibility. Say which
device, driver and operating system you actually tested in a hardware report.

## Golden vectors and decoder changes

[SPEC sections 6 and 7](docs/SPEC-decode-core.md) define determinism and the
golden vectors. The testkit generates fixtures from fixed seeds. For the
same binary, IQ input and configuration, repeated runs must produce
byte-identical spot logs. Cross-platform equality is checked at the decoded
text level; the spec does not require cross-platform FFT bit equality.

Keep the golden tests relevant to your change passing. Add a regression
case when fixing decoder behavior. Do not loosen a threshold, ignore a
test or regenerate expected output merely to make a regression disappear.
When deliberately changing a fixture, explain why and record its seeds,
generation settings and exact coppa revision. The current dependency pin
is in [Cargo.toml](Cargo.toml); the
[golden-vector freeze note](wiki/pages/golden-vector-freeze.md) explains why
fading fixtures need that provenance.

Some acceptance gates remain open and some tests are explicitly ignored.
A passing default test run does not mean every milestone has passed.
Consult [ROADMAP.md](ROADMAP.md), the relevant test and its linked decision
before reporting a gate as satisfied. Include any targeted acceptance runs
and their actual results in the PR.

If a change alters decoder output, add a `### Decoder output` entry under
`## [Unreleased]` in [CHANGELOG.md](CHANGELOG.md). Such a change requires at
least a MINOR version increase for the next release. PATCH releases do not
change decoder output. The version field used to identify a build is not
itself decoder output.

## Pull requests and review

Work on a branch or fork. For work in progress, open a draft PR early so
others can see what you are doing. Main changes through reviewed PRs with
passing required CI. Describe the problem, the resulting behavior and the
checks you ran. Use the PR template to link the MAN ticket or public issue.
Mark a draft ready when it is ready for review; a draft from a trusted
in-repository author does not receive the normal CI run until then.

The [review convergence policy](docs/DECISIONS/2026-08-07-pr-review-convergence-policy.md)
applies across human and automated reviews:

- Address all reasonable findings in round one.
- Fix P1 findings in every round. Correctness, security, broken builds and
  spec violations count as P1 here, even if a bot labels them P2.
- From round two, put genuinely new or reopened P2-and-lower findings
  verbatim in one follow-up ticket per PR. Link that ticket in the review
  thread before resolving it. Ask a maintainer to create the ticket if you
  lack tracker access.
- An unfinished round-one fix is still owed inline. Filing a ticket also
  does not replace a human reviewer's sign-off on requested changes.

## Changelog and releases

Add notable changes to the existing `Unreleased` section of
[CHANGELOG.md](CHANGELOG.md). Maintainers follow the
[release runbook](docs/RUNBOOKS/release.md#cutting-a-release), including
moving those entries into a dated version section before tagging. A
documentation PR does not create a release or invent a release date.

## License

manta is licensed under MIT OR Apache-2.0, at your option. Read the
[MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) texts and the contribution
clause in the [README](README.md#license).

GitHub's repository page can show both detected licenses. Its community
profile API returns one `files.license` object, so an Apache-2.0-only value
there does not describe the full choice offered by this project. See
[GitHub's multiple-license display](https://github.blog/changelog/2022-05-26-easily-discover-and-navigate-to-multiple-licenses-in-repositories/)
and [community metrics API](https://docs.github.com/en/rest/metrics/community).
