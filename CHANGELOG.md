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

`0.1.0` is the first tagged release; its GitHub Release notes list every
pull request merged before it. Changes before this file started are also in
the git history and `docs/DECISIONS/`.

## [Unreleased]

### Decoder output

- MAN-95: default spot validation requires 2, 3 or 5 distinct-message
  repetitions by callsign-pattern rarity. `[spot].validation_level = "minimal"`
  restores flat 2; `--patterns` overrides the independent reference list.

- A band-wide static crash (lightning) no longer starts tracks across the
  band, and running tracks no longer decode it as extra dits; new
  `[detector].crash_filter`, default `"full"` (MAN-312).
- MAN-314: RTTY (two-tone FSK, 170/200/425/450/850 Hz shift) is recognised
  after 20 s and no CW track starts on it or its keying sidebands; open
  tracks on it close (`rtty` close count). `[detector] rtty_filter = false`
  restores the previous behaviour.
- MAN-315: the `legacy` decoder learns each track's own dot, dash and gap
  lengths. Gaps are measured against the sender's dit, and the word
  boundary is the sender's own, so letter gaps up to about 4.5 dits no
  longer split words. Dashes up to 6 dots long are accepted (was 4.5).

### Added

- MAN-315: a `TimingUpdate` event per track (each class's mean, variance
  and count, in dits), a top-level `timing` object in `decode --json`, and
  `manta decode --timing` for a `timing (dits, mean ± sd): …` stderr line.

- `manta completions <shell>` prints a tab-completion script for bash, zsh,
  fish, PowerShell or elvish; `manta man-pages --out <dir>` writes `manta.1`
  and one page per command, and every release archive carries them in
  `man/man1` (MAN-133).
- `manta_static_crashes_total` metric; `[detector].crash_filter`
  (`full`/`starts-only`/`off`) (MAN-312).
- `[detector] rtty_filter`/`rtty_dwell_ms`; `manta_rtty_signals{band}`
  gauge; `rtty` array in `decode --json` and, when RTTY was marked, an
  `rtty:` line in plain `decode`; `RTTY marked`/`RTTY cleared` log lines in
  `run` (MAN-314).
- A live-state stream for dashboards, off unless `[server].live_port` is
  set (`live_bind_addr` defaults to `127.0.0.1`): `bandState` (per-band
  noise floor, occupancy and decoding tracks, every second), `trackState`
  (each track's frequency, speed, SNR, decoded text and candidate calls),
  `candidateDecision` (every validation decision with its reason, and the
  spot's `id` when spotted) and `trackClosed` records, as JSON Lines or
  WebSocket frames on their own port. `/metrics` gains
  `manta_live_clients_connected`, `manta_live_records_total{kind}`,
  `manta_live_records_dropped_total{reason}` and
  `manta_listener_up{listener="live"}` when it is on. Spot output is
  unchanged. See `docs/LIVE-STATE.md` (MAN-324).
- A Grafana dashboard (spot rate, active tracks, source health, per-target
  uplink status) and Prometheus alert rules (node down, node unhealthy, no
  spots for 30 minutes, uplink target down or flapping) for a node's
  `/metrics`, with promtool tests. See `docs/RUNBOOKS/monitoring.md`
  (MAN-129).
- MAN-140: `--ka9q-group` / `[input] type = "ka9q"` reads one ka9q-radio
  (radiod) IQ channel over RTP multicast; see docs/GUIDES/ka9q-radio.md

### Changed

- The in-repo `wiki/` is the project's canonical wiki. GitHub's Wiki tab
  is not used; disabling it remains an owner step. The wiki's stale
  `verified` stamps and `manifest.json` are refreshed and CI-pinned (MAN-152).

## [0.1.0] - 2026-10-11

Pre-stability alpha, expect breakage: manta has not cleared its own M2/M3
acceptance gates.

### Decoder output

- Strong signals (about +24 dB SNR or more in 2500 Hz) no longer spawn
  duplicate tracks on adjacent channels that take over their decoder every
  fraction of a second. They retain one continuous track and recover their
  text and frequency (MAN-172). HSMM's VR1/VR2 reported-speed gates remain
  open.

### Added

- Prebuilt archives for Linux (x86-64, arm64), macOS (x86-64, arm64) and
  Windows (x86-64) on each GitHub Release, with `SHA256SUMS` and a
  build-provenance attestation (MAN-298).
- `manta health` asks a running daemon's `/healthz` and exits 0 when it
  reports healthy, 1 otherwise, reading the metrics port from the same config
  and `MANTA_*` overrides as `run` (MAN-81).
- The Docker image runs the daemon from a config mounted at
  `/etc/manta/manta.toml` (or `MANTA_CONFIG`) when started with no arguments,
  instead of printing `--help`, and carries a `HEALTHCHECK` that runs
  `manta health`. Arguments still run `manta <arguments>` unchanged (MAN-81).
- A `linux/arm/v7` Docker image for 32-bit Raspberry Pi OS, alongside
  `linux/amd64` and `linux/arm64` (MAN-81).
- `manta calibrate` measures the receiver's frequency error against a WWV,
  WWVH, BPM, RWM or NCDXF beacon carrier and, after confirmation, saves
  `freq_correction_ppm` to `[input]` in the config file (MAN-127).
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

- `sh/dx` history now retains every spot heard in the last
  `[server].history_minutes` minutes (default 30; at least the 50 newest,
  at most 10,000). `manta_spots_history_truncated_total` counts spots the
  ceiling drops early (MAN-93). Bare `sh/dx` still shows the last 10;
  loggers can request `sh/dx 30m`.
- Command output reads as finished text rather than debug prints (MAN-270).
  Human reports use shared units: frequencies as `14012.3 kHz`, SNR as
  integer `dB`, speed as integer `WPM`, confidence to two decimals, and spot
  types as `CQ`/`DE`/`BEACON`/`unknown`; for example
  `SPOT: W1AW (CQ) 14000.8 kHz 29 dB 20 WPM conf=0.79`. An application error
  is one escaped `error: ...` line on stderr instead of Rust's `Error:` chain
  (still exit 1). `decode` prints its summary on stderr, `soak` and `doctor`
  print plain reports, and `run`'s startup banner shows
  `dial_frequency=14060.0 kHz`. JSON output, the RBN wire format and decoder
  output are unchanged. See
  [docs/DECISIONS/2026-10-10-man270-cli-output-style.md](docs/DECISIONS/2026-10-10-man270-cli-output-style.md).
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
