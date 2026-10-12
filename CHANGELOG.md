# Changelog

Notable changes to manta. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) plus one rule for
decoder output.

**Decoder output rule.** Decoder output is the set of spots manta produces
from a given input file and configuration: which calls are spotted, when, and
at what frequency, SNR, speed, spot type and confidence. That is what
`manta decode --json` and the spot lines of `manta run --json` record. A
change that alters it is listed under `### Decoder output` in
`## [Unreleased]`, and the next release must then bump at least the MINOR
version. A PATCH release never changes decoder output. The JSON stream's
`decoderVersion` (`manta-<version>+<commit>`) is build identity, not decoder
output. See
[docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md](docs/DECISIONS/2026-10-10-man83-build-identity-and-decoder-versioning.md).

No release has been tagged yet. Changes before this file started are in the
git history and `docs/DECISIONS/`.

## [Unreleased]

### Decoder output

- Strong signals (about +24 dB SNR or more in 2500 Hz) no longer spawn
  duplicate tracks on adjacent channels that take over their decoder every
  fraction of a second. They retain one continuous track and recover their
  text and frequency (MAN-172). HSMM's VR1/VR2 reported-speed gates remain
  open.

### Added

- `manta doctor` checks the whole setup before its signal check: the config
  (`manta run`'s example-callsign and dial-frequency refusals), the audio
  library and sound-card inputs, that each `[server]` port is free, the
  clock against an NTP server (and, on Linux, the kernel's NTP sync), and
  that each enabled `[[rbn_uplink]]` target accepts a connection. Each
  check prints `PASS`, `WARN`, `FAIL` or `SKIP`, every problem with a
  `fix:` line, followed by a summary naming each failed and warned check
  (MAN-126). See `docs/RUNBOOKS/setup-checks.md`.
- `manta doctor --ntp-server HOST[:PORT]` names the clock check's server
  (default `pool.ntp.org`).
- `manta doctor --json` adds `checks` and `checks_status`; every existing
  key is unchanged.
- `manta doctor` exits 1 when any check fails. Scripts may now see exit 1
  for a held port, an unreachable uplink, a clock 60 s or more off, the
  example callsign, or a missing dial frequency with `[server]`. The signal
  verdict still does not change the exit code.
- Operator guides for [running an RBN node](docs/GUIDES/rbn-node.md),
  [KiwiSDR etiquette](docs/GUIDES/kiwisdr.md), and
  [Raspberry Pi installation and service setup](docs/GUIDES/raspberry-pi.md) (MAN-148).
- `manta run` takes `-v`/`-vv` and `-q`/`-qq`/`-qqq` and
  `--log-level <LEVEL>`, each shorthand for `RUST_LOG=<level>` that
  overrides it, and `--log-format json`, which writes every stderr log line
  as one JSON object for a log aggregator (MAN-124).
- Contributor setup and review guidance, bug and hardware report templates,
  feature requests, and a MAN-linked pull request template (MAN-151).
- `manta decode --center-freq-hz HZ` sets the recording's RF centre
  frequency, overriding its `<name>.json` sidecar. Without the flag, a
  recording with no sidecar (or a sidecar whose `center_freq_hz` is not
  positive) gets a warning that its frequencies are baseband offsets,
  and is decoded at a 0.0 centre (MAN-131).
- Live comparison with the Reverse Beacon Network (MAN-301). With a new
  `[rbn_compare]` table enabled, a `[server]` daemon reads RBN's public CW
  telnet feed and grades each spot, once a window around its time closes
  (`window_secs`, default 180 s), as `match`, `disagree`, `elsewhere`,
  `none` or `unknown`. Each grade is a `spotGrade` record on the `:7301`
  JSON stream, referencing the spot's `id`, and is counted in new
  `manta_rbn_compare_*` metric families: per-band grade counts over the
  last hour and the per-band median frequency offset from RBN. manta only
  reads the feed: it sends its login callsign and nothing else. The table
  is off by default; without it the JSON stream and `/metrics` are
  unchanged and no connection is made. Spots themselves do not change.
  See `docs/RUNBOOKS/rbn-comparison.md`.
- `manta run --rbn-compare` turns comparison on for one run, and
  `--rbn-spotter CALL` (repeatable) grades against only that RBN skimmer,
  for example a co-located one.
- `manta grade` grades a recorded node's spot log (`field-node.py
  record-spots`'s `spots-*.jsonl`) against RBN daily-archive CSVs with the
  daemon's grading code, printing a per-band summary or, with `--json`,
  the same `spotGrade` records the live stream carries.
- `scripts/score-against-rbn.py --node-log` replays a node's spot log
  through `manta grade`, unzipping RBN's archive first; `--spotter K5TR`
  restricts it to one skimmer as before.
- `manta doctor` checks that `[rbn_compare]`'s feed accepts a TCP
  connection (`rbn compare feed`), sending nothing on it.
- `field-node.py record-spots` writes grade lines to
  `grades-<UTC day>.jsonl`, and `shadow-compare.py` skips grade lines
  instead of counting them as spots.

### Fixed

- `manta run`'s log lines carry no colour escape codes when stderr is not a
  terminal (a file, a pipe, the journal or a container log); `NO_COLOR` is
  still honoured on a terminal (MAN-124).
- `manta run`/`listen`/`soak`/`doctor --device` keep running when the
  sound card's capture buffer is momentarily empty, instead of stopping
  with exit 0 or `audio source ended during startup calibration`. A device
  that delivers nothing for 5 s is reported as an error (MAN-231).
- Audio device errors name the device and the 48000 Hz requirement and
  say what to check next, with a macOS microphone-permission hint, and a
  device delivering only digital silence gets a warning (MAN-131).
  Decoder output is unchanged.

### Changed

- `manta doctor` no longer prints `note: doctor does not start the spot
  servers; ignoring [server]`: it checks `[server]`'s ports instead, still
  starting no server.
- A receiver that `manta doctor` cannot open is a named `FAIL  receiver:`
  line with a fix rather than an `Error:` chain (still exit 1). With
  `--json`, doctor then prints an object holding only `checks` and
  `checks_status` instead of nothing.
- JSON spots' `deGrid`/`deLat`/`deLon` are the operator's configured
  station position (`[server].operator_grid`, or the new `operator_lat`/
  `operator_lon`), and `null` when none is configured, instead of the
  station's country centroid from `cty.dat` (MAN-138). Decoder output is
  unchanged.
- `manta --version` (and `-V`) names the exact build: crate version, git
  commit and compiled-in features, for example
  `manta 0.1.0 (git 1a2b3c4d5e6f; features: hpsdr)` (MAN-83).
- The JSON spot stream's `decoderVersion` carries the git commit as SemVer
  build metadata, for example `manta-0.1.0+1a2b3c4d5e6f`, instead of
  `manta-0.1.0` for every build (MAN-83). Decoder output is unchanged.
- `manta run` prints decoded text on stderr, one labelled line per track,
  instead of interleaving every track's characters on stdout. `SPOT:` lines
  move from stderr to stdout. With a `[server]` table decoded text is off
  unless `--decoded-text` is given (MAN-123). Decoder output is unchanged.
