# MAN-82: a SoapySDR release archive for Linux x86_64

Date: 2026-10-14
Status: implemented in the branch. A hosted release run, the published
archive and a run with a real receiver are still outstanding (see
"Verification limits").

README's quickstart named `--soapy-driver`, but every release binary was
built with `--features hpsdr` only, so an RTL-SDR, Airspy, SDRplay or
HackRF operator needed a Rust toolchain and a source build. A downloaded
binary rejected the flag with clap's `error: unexpected argument
'--soapy-driver' found` (exit 2).

## Decisions

### D1: one extra archive, not Soapy in the default archives

Releases gain a sixth archive, `manta-linux-x86_64-soapy.tar.gz`. The five
existing archives keep their names, their `hpsdr`-only feature set and their
build configuration (`cross` for both Linux legs).

- **Why.** The `soapy` feature links `libSoapySDR.so.0.8`, which the
  loader must find before `main` runs. Adding it to the default Linux
  archive would stop that binary starting on every machine without
  SoapySDR, including KiwiSDR, HPSDR and audio users who never need it.
- **Rejected: a Soapy leg in the `build` matrix.** The matrix legs share
  one Package step, one `man-pages` download and one feature set, and the
  Linux legs use `cross`. A separate `build-soapy` job with literal target
  and artifact names keeps those five legs unchanged.

### D2: direct dynamic linking; the remedy for a missing library is documentation

The binary links the distro's SoapySDR through the existing `soapysdr`
crate (`crates/manta-input/Cargo.toml`). No Cargo dependency changes.

- **Consequence.** Without `libSoapySDR.so.0.8`, the loader refuses to
  start the binary, even for `--version`, with `error while loading shared
  libraries: libSoapySDR.so.0.8: cannot open shared object file` and exit
  127. manta never runs, so it cannot print its own "install libsoapysdr"
  message. README's "SoapySDR on Linux x86_64" quotes the loader's message
  and says to run `sudo apt-get install libsoapysdr0.8`.
- **Rejected: lazy loading (`dlopen`) or a launcher script.** Either would
  let manta print its own error, but means replacing the binding with
  hand-written FFI or adding a second entry point. That is larger than this
  ticket and changes the input path the existing tests cover.
- **Rejected: bundling the library.** SoapySDR loads device modules from
  system paths at run time, and the SDRplay API is proprietary. A bundled
  core library would still need system modules, and the archive would
  carry a second copy to keep patched.

### D3: Ubuntu 24.04 x86_64 is the supported baseline

`build-soapy` runs on `ubuntu-24.04` (pinned, not `ubuntu-latest`) and
installs `libsoapysdr-dev` from the distro. The binary therefore needs
Ubuntu 24.04's glibc and SoapySDR 0.8 ABI. README documents Ubuntu 24.04
x86_64 for this archive only and does not raise the default archive's
baseline. Ubuntu 22.04 also packages SoapySDR 0.8, but its older glibc
cannot be assumed to run a 24.04-built binary, so it is not claimed.

### D4: device modules and vendor software stay external

The archive bundles neither the vendor API nor driver modules. Operators
install `libsoapysdr0.8` and their receiver's module
(`soapysdr0.8-module-rtlsdr`, `-airspy`, `-hackrf`). SDRplay needs
SDRplay's own API and a compatible SoapySDRPlay3 module, following
[SoapySDRPlay3's requirements](https://github.com/pothosware/SoapySDRPlay3/blob/master/README.md);
building that module may need a C++ compiler, but running manta needs no
Rust toolchain.

### D5: the Release waits for the Soapy archive; the image does not

`release-publish.yml`'s `release` job has `needs: [validate-tag, build,
build-soapy]`, so under ordinary success semantics a failed or skipped
Soapy build publishes no Release. The `manta-*` download pattern, the
checksum step and the attestation now cover six archives without other
changes. `docker-publish-release`, `docker-build-dispatch` and the
build-only `docker-build` keep their existing `needs`; MAN-298's
separation of the Release from the image outcome is unchanged. Build jobs,
including `build-soapy`, still cannot mint a signing token (MAN-80).

### D6: the archive's man pages come from its own binary

`validate-tag` renders the `build` legs' pages from an `hpsdr` binary, so
they omit the Soapy flags. `build-soapy` runs `manta man-pages --out
man-pages-soapy` with the binary it packages and does not download the
`man-pages` artifact.

### D7: the packaged archive is checked before upload, twice

`scripts/check-soapy-release.py` extracts the archive into a temporary
directory (refusing members outside its root, links and devices) and runs
only the extracted binary. It checks that `--version` lists `hpsdr,soapy`,
`run`/`listen`/`check --help` offer the four Soapy flags and
`--hpsdr-host`, `manta-run.1` and `manta-check.1` document the Soapy
flags, a nonexistent driver fails with SoapySDR's `no match` (exit 1, not
exit 2 or 127), and `devices --json` enumerates Soapy without error. Each
invocation has a timeout. `build-soapy` runs it on the runner, then in a
fresh `ubuntu:24.04` container that mounts only the archive and the
verifier read-only. In the container it first expects the loader's
missing-library failure, then installs `libsoapysdr0.8` and three distro
modules, and checks that no Rust, compiler or `-dev` package is present
before the full check. The job also runs the three real-library
`soapy::tests` with the combined features. Any failure stops the upload.

## Measured in the implementing container (2026-10-14)

Ubuntu 24.04 x86_64, no root, no Docker, no receiver. `libsoapysdr0.8` and
`libsoapysdr-dev` 0.8.1-4build1 were extracted with `apt-get download` and
`dpkg-deb -x` into a temporary prefix, used through `PKG_CONFIG_PATH` and
`LD_LIBRARY_PATH` (no RPATH, nothing committed).

- `cargo build --release --locked -p manta-cli --features hpsdr,soapy
  --target x86_64-unknown-linux-gnu`: exit 0. `--version`: `manta 0.1.0
  (git 9636fe0494b4; features: hpsdr,soapy)`.
- `cargo test --release --locked -p manta-input --features hpsdr,soapy
  --lib soapy::tests`: 3 passed.
- The binary's 20 man pages, packaged by `scripts/package-release.py`:
  the verifier passed every check with the runtime library on
  `LD_LIBRARY_PATH`. Without it, `--expect-missing-soapy-library` passed
  (exit 127, `libSoapySDR.so.0.8: cannot open shared object file`) and the
  full check failed. The archive's SHA-256 was unchanged after both runs.
- `apt-get -s install --no-install-recommends python3 libasound2t64
  libsoapysdr0.8 soapysdr0.8-module-rtlsdr soapysdr0.8-module-airspy
  soapysdr0.8-module-hackrf` installs no `-dev` package.

## Verification limits

- The `ubuntu:24.04` container step did not run here (no Docker). It is
  committed in both workflows and runs on the hosted release runner; a
  `release.yml` dispatch from the branch exercises it without a tag.
- No receiver was attached. The null-device tests and the no-match check
  prove the binary reaches the real library, not that it streams IQ from
  hardware. A run with an RTL-SDR, Airspy, HackRF and SDRplay
  (vendor API and SoapySDRPlay3 installed) is still outstanding; record
  model, module/API versions, rates and result when one happens.
- Nothing was published. After the next release, verify the downloaded
  Soapy archive with `SHA256SUMS` and `gh attestation verify`, then repeat
  the verifier on it (docs/RUNBOOKS/release.md, "The SoapySDR archive").
