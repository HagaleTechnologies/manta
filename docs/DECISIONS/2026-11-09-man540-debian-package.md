# MAN-540: a Debian package for Debian, Ubuntu and 64-bit Raspberry Pi OS

Date: 2026-11-09
Status: implemented in the branch. A hosted release run and a Raspberry Pi
install are outstanding (see "Verification limits").

A Linux operator installed manta by unpacking a release archive, running
`install` for the binary and the config, copying
`packaging/systemd/manta.service` into `/etc/systemd/system/` and running
`daemon-reload` and `enable --now` by hand (`packaging/README.md`, "Linux:
systemd"). Nothing in the tree built a Debian package, and the MAN-268
record left package managers out of scope.

After this change every Release carries two more files beside the seven
archives: `manta-linux-x86_64.deb` (amd64) and `manta-linux-arm64.deb`
(arm64). `sudo apt install ./manta-linux-<arch>.deb` on Debian 12 or later,
Ubuntu 24.04 or 64-bit Raspberry Pi OS bookworm installs `/usr/bin/manta`,
the man pages, `/usr/lib/systemd/system/manta.service` (enabled, not started)
and a 0600 `/etc/manta/manta.toml` equal to `manta config init` output.
`SHA256SUMS` and the build-provenance attestation (MAN-80) cover both.

## Decisions

### A1: build with `dpkg-deb` from a stdlib helper, not `cargo-deb` (P1)

`scripts/package-deb.py` stages the files and runs
`dpkg-deb --root-owner-group -Zxz --build`. `dpkg-deb` is already on every
Linux runner and Debian container, so pull-request CI builds and installs
real packages, and the release path compiles no extra third-party tool.
`--root-owner-group` gives every member `root/root` with the staged modes
as a non-root user, so neither `fakeroot` nor root is needed. `-Zxz` keeps
the package readable by Debian 11's dpkg: Ubuntu 24.04's `dpkg-deb` writes
zstd members by default. The cost is the debhelper 13.14 systemd snippets
copied by hand into `packaging/deb/` (about 30 lines). This reverses the
research's D1.

### A2: build in `build`'s Linux legs from the archive's binary, with stable names (P2, P3)

The two Linux legs of the existing `build` matrix run "Package (.deb)"
after the archive's Package step and upload `<artifact>.deb` in the same
`manta-linux-*` artifact. `release` downloads `manta-*`, so it checksums,
attests and publishes the packages with no new job or step, and
`release.needs` is unchanged. The package's `/usr/bin/manta` is the leg's
`target/<triple>/release/manta`, copied byte for byte and never stripped,
so the `.deb` and the `.tar.gz` ship the same binary. The names follow the
archives' (`manta-linux-arm64.deb`), so README's
`releases/latest/download/<name>` links work for packages too; the version
lives in the control file.

### A3: the transient `manta` user, no static user, no shipped cache (P4)

The packaged unit keeps `DynamicUser=yes`: systemd allocates a transient
user named `manta` each time the service runs, and `CacheDirectory=manta`
creates `/var/cache/private/manta` with the `/var/cache/manta` link when the
service first starts. A static user would change where `CacheDirectory=`
puts the cache, would collide with field-node hosts that already have a
static `manta` user (`docs/RUNBOOKS/secondary-skimmer-field-node.md`), and
would make systemd use that user for `manta.service` too (systemd.exec,
`DynamicUser=`). MAN-268 D5 chose `DynamicUser=`. The maintainer scripts
run no `adduser`, and the package ships no `/var/cache/manta`. This is where
the ticket's wording ("the `manta` system user", "`/var/cache/manta` ...
exist" after install) changes.

### A4: the kit unit with one line changed (P5)

The packaged `/usr/lib/systemd/system/manta.service` is
`packaging/systemd/manta.service` with `ExecStart=/usr/local/bin/manta `
replaced by `ExecStart=/usr/bin/manta `. The helper refuses to build unless
exactly one line starts with the kit prefix, so a unit edit that adds or
removes an `ExecStart=` fails the build instead of shipping a wrong path.
The archive's unit and its pinned test are unchanged.

### A5: `Depends` on ALSA, `Breaks` old systemd (P6)

`Depends: libasound2 (>= 1.0.16) | libasound2t64` covers Debian bookworm
and Raspberry Pi OS (`libasound2`) and Ubuntu 24.04 and Debian 13
(`libasound2t64`, which provides `libasound2`). `Breaks: systemd (<< 247)`
refuses hosts whose systemd cannot expand `${CREDENTIALS_DIRECTORY}` in
`ExecStart` (the planning prototype refused a fake systemd 245 with
`manta breaks systemd (<< 247)`). There is no `libc6` dependency: both
binaries are cross-built for a glibc floor below every host that passes the
`Breaks`, and the bookworm container check would catch a regression.

### A6: enable on install, never start, try-restart on upgrade, stop on remove (P7)

The scaffold's `N0CALL` callsign and missing receiver stop manta at once,
and `Restart=always` would start it again every 10 seconds, so the package
enables the service but never starts it. On upgrade the postinst runs
`deb-systemd-invoke try-restart`, so a running service restarts on the new
binary and a stopped one stays stopped. The prerm stops the service only on
`remove`. The postinst's systemd blocks are debhelper 13.14's
`postinst-systemd-enable` and `postinst-systemd-restartnostart`
(`RESTART_ACTION=try-restart`); the prerm is `prerm-systemd-restart`; the
postrm is `postrm-systemd-reload-only` and `postrm-systemd`. Every path the
scripts write sits under `${DPKG_ROOT:-}`, so a scratch-root install in the
tests touches only that root.

### A7: `/etc/manta/manta.toml` is created by the postinst, not a conffile (P8)

The scaffold, `crates/manta-cli/src/config_init.toml`, changed in 52
commits since 2026-10-01, and every operator must edit it before the service
does anything. As a conffile, nearly every upgrade would stop at dpkg's
conffile prompt; a non-interactive install fails there with
`end of file on stdin at conffile prompt`, leaving the package unpacked and
the service not restarted (reproduced in planning with a `dpkg-deb`-built
package). Instead the package ships the scaffold at
`/usr/share/manta/manta.toml`, and the postinst copies it to
`/etc/manta/manta.toml` at mode 0600 only when no file exists. No upgrade
replaces it or prompts about it, and `apt purge manta` deletes it. An
operator who wants the new release's settings runs
`manta config init --out - | sudo diff - /etc/manta/manta.toml`. This reverses
the research's D10 and is where the ticket's "(conffile)" changes.

### A8: check the packages where they will run, and test real packages on every pull request (P9, P10)

At release time each Linux leg installs, reinstalls over an edited config
and purges its package in a clean `debian:bookworm` container (arm64 under
QEMU), and the x86_64 leg installs the amd64 package on the GitHub-hosted
runner, whose PID 1 is systemd: `scripts/check-deb.py --systemd` checks the
service is enabled but not started, starts it on a looping IQ recording,
reads back the sandbox `DynamicUser=yes` implies (`packaging/README.md`,
"What the service may do"), and checks that a reinstall restarts it and
keeps the config. No sandbox directive is added to the unit: it would
change the archive unit too, which MAN-268 records as never run on a real
systemd host. On every pull request, `scripts/tests/test_package_deb.py`
builds real packages from fake binaries with `dpkg-deb` and installs,
upgrades, migrates, removes and purges them in a scratch dpkg root on Linux.
This replaces the research's D13, which only parsed metadata.

### A9: the package version comes from the tag; SoapySDR packages wait (P11, P12)

`v0.2.0` gives `0.2.0-1`, `v0.2.0-rc.1` gives `0.2.0~rc.1-1` (sorted before
`0.2.0-1` by `dpkg --compare-versions`), and a `workflow_dispatch`
rehearsal, whose version is `dispatch-<run_id>`, gives
`0~dispatch.<run_id>-1`, which sorts below every release. No gate compares
the tag with `Cargo.toml`. SoapySDR packages move to a follow-up ticket: the
unit's transient user is in no device group, so a packaged service cannot
open a USB receiver, which is the only reason to choose the SoapySDR build,
and the x86_64 SoapySDR binary is built for Ubuntu 24.04, not Debian.

## Superseded in part

[MAN-268's record](2026-10-10-man268-unattended-packaging.md), "Out of
scope", lists package managers (deb, rpm, Homebrew). This record supersedes
that line for Debian packages only; rpm and Homebrew stay out of scope.

## Verification limits

- The planning container had no root, no systemd, no Docker and no `apt`
  install rights. Not run: a real systemd start of the packaged unit,
  `systemctl show` of the DynamicUser-implied properties, `apt install` as
  root, a Debian bookworm or Raspberry Pi OS install, a real aarch64 binary
  through the helper, the QEMU container and lintian. The `systemctl show`
  values follow systemd's `unit_patch_contexts()`, which sets them on the
  loaded unit when `DynamicUser=yes`; the first rehearsal run is the first
  observation.
- Outstanding: a maintainer dispatch of `release-publish.yml` from the
  branch (`gh workflow run release-publish.yml --repo
  HagaleTechnologies/manta --ref MAN-540 -f publish=false`). Both Linux legs
  must pass the three new check steps, and the x86_64 leg's log must show
  every `PASS` line of `scripts/check-deb.py --systemd`, including the
  sandbox values.
- Outstanding: on a Raspberry Pi 4 or 5 with 64-bit Raspberry Pi OS
  bookworm, `sudo apt install ./manta-linux-arm64.deb` from the rehearsal
  artifact prints the four `manta:` lines; `systemctl is-enabled manta` is
  `enabled` and `is-active` is `inactive`; after setting a KiwiSDR or HPSDR
  receiver, `sudo systemctl start manta` and the `/healthz` probe return
  `200`; `ls -l /var/cache/manta` shows the `private/manta` link. A second
  package built with `--version 0.1.1` installs over it with no prompt,
  keeps the edited config byte for byte and changes `MainPID`.

## Follow-up tickets

These need a person with Linear access.

1. "A Raspberry Pi operator with a USB SDR should install manta-soapy with
   apt so that the packaged service can open the receiver." Build
   `manta-soapy` packages in `build-soapy-arm64` (bookworm) and
   `build-soapy` (Ubuntu 24.04), `Conflicts`/`Replaces`/`Provides: manta`,
   `Depends: libsoapysdr0.8`, `Recommends: soapysdr0.8-module-rtlsdr`;
   decide and test on hardware how the `DynamicUser=` service reaches
   `/dev/bus/usb` (for example `SupplementaryGroups=plugdev` in a packaged
   drop-in).
2. "An operator should get manta upgrades from `apt upgrade`" (ticket
   phase 2): a signed apt repository on GitHub Pages or Cloudflare R2 that
   indexes the Release's `.deb` files.
3. "A macOS operator should install manta with Homebrew" (ticket phase 3).
4. "The Debian package should pass lintian and ship bash, zsh and fish
   completions."

## References

- `scripts/package-deb.py`, `packaging/deb/`, `scripts/check-deb.py`,
  `scripts/tests/test_package_deb.py`.
- `.github/workflows/release-publish.yml`, `build` job.
- `packaging/README.md`: "Debian, Ubuntu and Raspberry Pi OS: the package",
  "What the service may do", "Moving from an archive install to the
  package".
- `docs/RUNBOOKS/release.md`, "The Debian packages".
- [MAN-80 record](2026-10-10-man80-release-checksums-attestation.md),
  [MAN-539 record](2026-11-08-man539-arm64-soapy-archive-and-image.md).
