# MAN-539: an arm64 SoapySDR release archive, and SoapySDR in the Docker image

Date: 2026-11-08
Status: implemented in the branch. A hosted release run, the published
artifacts and a run with a receiver are outstanding (see "Verification
limits").

The headline deployment, a headless Raspberry Pi at a remote antenna with a
cheap USB SDR, could not be installed from a release artifact. The arm64
archive and the multi-arch Docker image were built with default features,
and the only SoapySDR archive was for Ubuntu 24.04 x86_64
([MAN-82](2026-10-14-man82-soapy-release-archive.md)).
`docs/GUIDES/raspberry-pi.md` sent Pi operators to install Rust and build from
source. SoapySDR is manta's only route to RTL-SDR, Airspy, HackRF and SDRplay.

After this change the Release has a seventh archive,
`manta-linux-arm64-soapy.tar.gz`, for 64-bit Raspberry Pi OS bookworm and
later, and the one Docker image carries SoapySDR and three device modules on
every platform. `manta devices` on a build without SoapySDR names both
SoapySDR archives.

## Decisions

### A1: build the arm64 archive inside Debian bookworm, on a native arm64 runner

`build-soapy-arm64` runs on `ubuntu-24.04-arm` and builds and tests inside
`docker run … rust:1-slim-bookworm`, the Dockerfile's builder base. The
checkout's `rust-toolchain.toml` pins 1.98.0 inside it, as in the image
build. Bookworm is the claimed platform (glibc 2.36, `libsoapysdr0.8`
0.8.1-3), so the binary's glibc floor holds by construction. The container
runs as root over a runner-owned checkout, which `git` refuses to read, so the
job passes `MANTA_GIT_SHA="$GITHUB_SHA"` for the build ID
(`crates/manta-cli/build.rs`).

- **Rejected: a `Cross.toml` `pre-build` line.** The `cross` 0.2.5 aarch64
  image is Ubuntu 16.04 (glibc 2.23) and packages no SoapySDR, so no
  `pre-build` line can install `libsoapysdr-dev:arm64`.
- **Rejected: a build on `ubuntu-22.04-arm`.** Ubuntu 22.04's standard
  support ends in April 2027, and its runner label will not outlive that by
  much.
- **Rejected: a plain build on `ubuntu-24.04-arm`.** Against noble's glibc
  2.39, the release binary needs at most `GLIBC_2.35`, but the `manta-input`
  test binary needs `GLIBC_2.39` (`pidfd_spawnp`, `pidfd_getpid`, from Rust
  std's process spawning) and does not start on bookworm. The first
  `std::process::Command` in the shipped binary would break the archive on
  Pi OS.
- **Fallback.** If the `ubuntu-24.04-arm` label is unavailable to this
  repository, run the same job on `ubuntu-24.04` with
  `docker/setup-qemu-action` and `--platform linux/arm64` on both `docker
  run` lines. It is slower and gives the same result.

### A2: a separate job, not a matrix

`build-soapy-arm64` is its own job beside `build-soapy`. The arm64 job builds
in a container while `build-soapy` builds on the runner, so a matrix would
need step conditions on every leg. A separate job leaves `build-soapy` and
its pinned tests unchanged and follows MAN-82 D1's rule of literal target and
artifact names. The ticket's "when `build-soapy` runs for aarch64" is met by
`build-soapy-arm64`, which runs `cargo test --release --locked -p manta-input
--features soapy --lib soapy::tests` against bookworm's library.

### A3: verify only in a clean `debian:bookworm` container

The runner is Ubuntu 24.04 arm64: not the claimed platform, and without
SoapySDR. The job runs the verifier only in a fresh `debian:bookworm`
container that sees the archive and the verifier, read-only. It keeps MAN-82
D7's order: the loader failure with only `python3 libasound2` installed, then
every check after installing `libsoapysdr0.8` and the RTL-SDR, Airspy and
HackRF modules, with the no-compiler and no-`-dev` guards.

- **Rejected: an `objdump` glibc-ceiling step.** Building in bookworm fixes
  the floor by construction, and the clean bookworm run fails the job with
  the loader's own ``GLIBC_x not found`` if that ever changes.

### A4: one image with SoapySDR, checked before it is pushed

The Dockerfile builds `--features soapy` against Debian's `libsoapysdr-dev`
and installs `libsoapysdr0.8`, `soapysdr0.8-module-rtlsdr`,
`soapysdr0.8-module-airspy` and `soapysdr0.8-module-hackrf` in the runtime
stage, for `linux/amd64`, `linux/arm64` and `linux/arm/v7`. The runtime
packages add about 1.1 to 1.5 MB per platform. Each Docker job
(`docker-publish-release`, `docker-build-dispatch`) first builds
`linux/arm64` alone with `load: true` and runs the verifier's image mode on
it, then runs the unchanged three-platform build-push. BuildKit reuses the
first build's arm64 layers. Every image user now loads `libSoapySDR`, so a
runtime stage without it would publish an image in which nothing starts,
KiwiSDR users included; the gate stops that before the push.

- **Rejected: a second `-soapy` tag.** It would double the QEMU build time
  and the `:latest` surface for a 1.5 MB difference.
- **Not changed.** Image tags, `:latest`, environments and `publish-latest`.
  The Release still does not wait for the image (MAN-298, MAN-82 D5).

### A5: the verifier gains an image mode, not a second script

`scripts/check-soapy-release.py --docker-image IMAGE --platform P` runs
`docker run --rm --platform P IMAGE <args>` where archive mode runs the
extracted binary. It reuses the version, help, no-match and devices checks,
skips the man-page check (the image ships no man pages), and adds a module
check: `driver=rtlsdr`, `driver=airspy` and `driver=hackrf` must each exit 1
with `cannot open source` and without SoapySDR's `no match`. A loaded module
fails with its own error (`No RTL-SDR devices found!`, `Unable to open AirSpy
device`, `no hackrf device matches`); a missing one fails with `no match`.
Archive mode is unchanged apart from wording; it already verifies an archive
of either architecture, because the root directory comes from the file name.

### A6: docs use real commands

The ticket's `manta doctor --source soapy` is not a valid command: `--source`
is a WAV path in `doctor`. Enumeration is `manta devices`, and the receiver
check is `manta doctor --soapy-driver <selector> --soapy-freq-hz <hz>
--soapy-rate-hz <hz>`.

### A7: README keeps the x86_64 section and its anchor

`#soapysdr-on-linux-x86_64` is linked from the release runbook, the RBN-node
guide and a pinned test. README adds a sibling `### SoapySDR on Raspberry Pi
OS (arm64)` with the bookworm install lines, and points to the x86_64
section's module table and error messages instead of repeating them.

### A8: MAN-539 gets its own record

This record. The MAN-82 record gains a pointer to it; its D1 to D7 stay as
written. The arm64 archive is claimed for 64-bit Raspberry Pi OS bookworm
(Debian 12) and later only. The measured glibc floor (2.34) would also allow
Ubuntu 22.04+ arm64, but nothing verifies it there. No 32-bit (armhf)
SoapySDR archive is built; the `linux/arm/v7` image has SoapySDR because it
shares the Dockerfile. No SDRplay module (proprietary API, not packaged by
Debian) and no Airspy HF+ module (not in bookworm) is installed anywhere.

## Measured in the planning container (2026-11-08)

The planning session ran in an Ubuntu 24.04 x86_64 container with no root, no
Docker and no SDR. Sysroots were extracted from `.deb` files; Rust 1.98.0's
`aarch64-unknown-linux-gnu` and `armv7-unknown-linux-gnueabihf` targets built
`cargo build --release --locked -p manta-cli --features soapy --target
<triple>`, and `qemu-aarch64-static`/`qemu-arm-static` 8.2.2 ran the results
on Debian bookworm userlands.

- **E1.** aarch64 against an Ubuntu jammy sysroot, run on bookworm arm64
  (`libsoapysdr0.8` 0.8.1-3, rtlsdr/airspy/hackrf modules): `NEEDED`
  `libSoapySDR.so.0.8 libasound.so.2 libgcc_s.so.1 libc.so.6
  ld-linux-aarch64.so.1`, max `GLIBC_2.34`. `--version`: `manta 0.1.0 (git
  915db0e2383c; features: soapy)`. `devices --json`:
  `"soapy":{"status":"ok","devices":[],"error":null}`. A nonexistent driver:
  `SoapySDR::Device::make() no match`, exit 1. `doctor --soapy-driver
  driver=rtlsdr`: `No RTL-SDR devices found!` (module loaded). Without
  `libsoapysdr0.8`: loader error, exit 127. `soapy::tests`: 10 passed.
- **E2.** The same against an Ubuntu noble sysroot (glibc 2.39): release
  binary max `GLIBC_2.35`, runs on bookworm. The test binary needs
  `GLIBC_2.39` (`pidfd_getpid`, `pidfd_spawnp`); on bookworm:
  ``/lib/aarch64-linux-gnu/libc.so.6: version `GLIBC_2.39' not found``.
- **E3.** The same against a Debian bookworm arm64 sysroot (`libc6-dev`
  2.36, `libsoapysdr-dev` 0.8.1-3), the job's build environment: release and
  test binaries both max `GLIBC_2.34`; `soapy::tests` on bookworm: 10 passed.
- **E4.** armv7 against a bookworm armhf sysroot, run on bookworm armhf (the
  image's `linux/arm/v7`): `--version` lists `soapy`, `devices --json` Soapy
  `ok`, a nonexistent driver gives `no match` and exit 1, `soapy::tests`: 10
  passed.
- **E5.** E1's binary packaged with `scripts/package-release.py --artifact
  manta-linux-arm64-soapy` (56 members, 20 man pages). The verifier, run
  through a QEMU wrapper, printed all five `OK:` lines with the bookworm
  runtime, and `--expect-missing-soapy-library` passed (exit 127) without
  `libsoapysdr0.8`.
- **E6.** `apt-get -s install` against bookworm indexes: the clean runtime
  set `python3 libasound2 libsoapysdr0.8
  soapysdr0.8-module-{rtlsdr,airspy,hackrf}` installs no `-dev` package and no
  compiler. The image's runtime set resolves on arm64, armhf and amd64 to the
  same versions (`libsoapysdr0.8` 0.8.1-3, `soapysdr0.8-module-rtlsdr`
  0.3.3-1, `-airspy` 0.2.0-3+b1, `-hackrf` 0.3.3-5+b1). The builder set
  `libasound2-dev pkg-config git libsoapysdr-dev` resolves on all three.
- **E7.** Bookworm's `librtlsdr0` and `libhackrf0` udev rules set
  `MODE="0660", GROUP="plugdev"`; `libairspy0`'s sets `TAG+="uaccess"` only.
  `librtlsdr` detaches the DVB kernel driver itself.
- **E8.** `manta config check` on a `type = "soapy"` config with a default
  build: `error: input.type = "soapy" needs a manta built with --features
  soapy`, exit 1. A soapy build converts the config without opening the
  device, so it accepts it. `crates/manta-cli/tests/operator_guides.rs`
  checks the Pi guide's USB example against the build's own features.

## Verification limits

- No Docker daemon, GitHub runner, real `rust:1-slim-bookworm` or
  `debian:bookworm` image, arm64 or armv7 hardware, or SDR was available. The
  `ubuntu-24.04-arm` label could not be queried. The arm64 runtime claims rest
  on QEMU user-mode runs against real Debian bookworm libraries.
- Outstanding: a maintainer dispatch of `release-publish.yml` from the branch
  (`gh workflow run release-publish.yml --repo HagaleTechnologies/manta --ref
  MAN-539 -f publish=false`) to run `build-soapy-arm64` and the image check.
  If the label is unavailable, apply A1's fallback before merging.
- Outstanding: on a Pi 4 or Pi 5 with 64-bit Raspberry Pi OS bookworm,
  `libsoapysdr0.8`, `soapysdr0.8-module-rtlsdr` and an RTL-SDR, the archive's
  `manta devices` lists the receiver, `manta doctor --soapy-driver …` passes
  the receiver check, and a 60 s `manta run` produces spots. In the image,
  `docker run --rm --device /dev/bus/usb --group-add plugdev <image> devices`
  lists the receiver. Record model, module version, rates and result here
  when one happens.
- Nothing was published. After the next release, verify the downloaded arm64
  Soapy archive with `SHA256SUMS` and `gh attestation verify`, then repeat the
  verifier on it (docs/RUNBOOKS/release.md, "The SoapySDR archives").
