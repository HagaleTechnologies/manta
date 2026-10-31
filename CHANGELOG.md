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

- MAN-109: integrate two disjoint power windows for detector rise; retain the EMA for drop and SNR.
- MAN-402: looped dah-heavy text keyed at 25-40 WPM (`MO`, `OO`, `0000`,
  `T0M`) now decodes as its own words. The speed tracker read five equal
  dahs of 90-150 ms as dits, so `MO` at 33 WPM printed `H5555...` and `0000`
  at 37.5 WPM printed nothing. When the gaps between those five marks are a
  third of a mark long, the tracker now takes them as dahs. Text made only
  of single-dah characters (`TT`) is unchanged.
- MAN-108: a fading signal keeps one `track_id` through fades of up to
  15 s instead of respawning a new track each time the 5 s hang ran out.
  A track that has decoded goes DORMANT for `reacquire_ms` (10 s) after
  the hang and wakes under the same id with a fresh decoder; the text it
  decodes is unchanged, its spots carry one id, and the validator's
  per-track state (RST, `QRL?`) survives the fade. A track whose signal
  really ends now closes 15 s after the drop instead of 5 s.
- MAN-307: live HPSDR queue losses reset only the affected decode segment.
  Decimation resets its filter and forwards converted gaps, and reconnect
  wrappers preserve inner gaps, including an EOF with a pending gap. Clean
  file replay remains unchanged.
- MAN-265: a transmission that ends at the speed tracker's warm-up
  boundary (e.g. a beacon ID or a single exchange) now reports its
  gap-corrected WPM. The readiness mark's seed was computed before the
  buffered inter-element gaps were replayed, so short transmissions kept
  the mark-only estimate; the seed is replaced after the replay, and
  later live marks keep the same EMA smoothing.
- MAN-234: an input sample that is NaN, infinite or larger than 1e17 in
  magnitude is replaced with silence and counted instead of ending the run
  (exit 101) or silencing it; clean input decodes byte-identically.
- MAN-174: a deferred Beacon candidate on a track that merged into another
  track which closed in the same batch now resolves at the final survivor's
  close instead of being lost; one whose timestamp equals an existing
  spot's goes through normal dedupe; a merge keeps the newest eight pending
  candidates by time.
- MAN-192: the opt-in `hsmm` engine now reseeds its speed hypotheses
  after a committed `Silence` segment even when the keying-present gate
  never re-opens (interference holding it high through the gap), as
  SPEC v2 §4.7 always required, so a transmission resuming at a
  different speed competes with fresh hypotheses instead of decoding
  from the surviving speed alone. `decode.engine` remains `legacy` by
  default; output changes only for `hsmm` runs with such gate-stuck
  gaps.
- MAN-95: default spot validation requires 2, 3 or 5 distinct-message
  repetitions by callsign-pattern rarity. `[spot].validation_level = "minimal"`
  restores flat 2; `--patterns` overrides the independent reference list.
- MAN-190: the opt-in `hsmm` engine's evidence front end now confirms
  mark/space anchors from two actually computed hops instead of a
  forward-looking prediction, closing a missed-anchor race with the
  per-hop speed feedback. Evidence hops are released one hop later
  (`h+1` at constant speed, shared with `edge-legacy`); adaptive input
  can decode differently. The default `legacy` engine is unchanged.

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
- MAN-313: noise-like wideband data and swept carriers start no CW track,
  SSB voice starts far fewer (7 in band against 81 at +30 dB with the
  testkit voice, none spotted), and a track that stops keying closes 10 s after its last
  key-down edge unless a callsign was read. A new track is decoded only
  once it shows CW keying, so its `TrackPromoted` arrives 0.5–1.5 s of
  stream later, with the same `sample_ts`; decoded text and spots are
  unchanged on the golden vectors. `manta decode` on a non-empty input with
  no keyed CW now fails with `no signal found (no keyed CW signal in the
  input)`. `manta_active_tracks` counts only keyed CW tracks.
  `[detector] non_cw_filter = false` restores the previous behaviour.

### Added

- MAN-308: `[input].auto_attenuation` (default `false`) lets manta set a
  protocol-2 openHPSDR radio's ADC0 step attenuator: 2 s of reported ADC
  overload adds 6 dB (up to 30 dB) and 10 minutes without removes 6 dB.
  Each change is logged, and `/metrics` gains `manta_input_attenuation_db`
  and `manta_input_attenuation_changes_total`.
- MAN-309: `manta run --record-iq PATH` (or `record_iq` in each `[input]`/
  `[[input]]`) records the IQ it decodes, from any input, to a little-endian cf32 file plus `PATH.meta.jsonl` (rate,
  centre, start time, source passband and guards, input gaps). `decode`
  and `run --source PATH --source-iq` replay it to the same output on every
  run, and both reproduce its input gaps: `decode` now closes its tracks
  at each gap and continues on a fresh segment timed after the missing
  samples. Both commands also read headerless cf32/ci16 files with
  `--iq-format` and `--sample-rate-hz` (or `[input] iq_format`/
  `sample_rate_hz`). Decoder output for existing gap-free WAV inputs is
  unchanged; only sources that report gaps gain the segmented behaviour,
  and a recording of rig audio keeps its analytic guard under `decode`. See
  [docs/GUIDES/iq-recording.md](docs/GUIDES/iq-recording.md).
- MAN-307: several HPSDR inputs can share one radio, with one independently
  tuned receiver per band, capacity probing for HL2, and an optional
  `receivers` declaration for other gateware. Each radio reconnects as a
  group. Bounded per-receiver queues report exact discarded samples through
  `manta_input_queue_dropped_samples_total`. A shared radio speaks protocol 1
  (its entries' `protocol` must match, and `"2"` is refused); a radio with
  one entry keeps MAN-305's protocol choice.
- Rust API: `manta_input::hpsdr::probe_receiver_capacity`, radio session
  retirement/timing handles, and `InputHealthCounters::queue_dropped_samples`.
  Exhaustive `manta_server::metrics::InputHealth` literals must initialize the
  new `queue_dropped_samples` field or use `Default`.
- MAN-305: protocol-2 openHPSDR radios (ANAN-class: Hermes, Angelia,
  Orion, Orion II, Saturn/G2) as inputs, alongside protocol 1 (Hermes-Lite
  2). `--hpsdr-protocol auto|1|2`, `[input] protocol` (`"auto"`, `"1"`,
  `"2"` or the integers 1 and 2) and `MANTA_INPUT_PROTOCOL` choose it; the
  default `auto` sends one directed discovery probe to the configured host
  (never a broadcast), uses protocol 2 when the radio answers as protocol 2
  and protocol 1 otherwise, and prints which on every open. `manta config
  check` prints `protocol=`. A protocol-2 radio sends IQ from port + 11
  (1035 by default), so the host firewall must allow inbound UDP from it;
  see docs/CONFIG.md. Its `manta_input_dropped_packets_total{source="hpsdr"}`
  counts datagrams proven lost by sequence number.
- MAN-305: `IqSource::take_missing_samples()`, a defaulted hook through
  which a source reports samples lost before a read and spliced over.
  `listen` advances its sample clock by that many samples without closing
  tracks, so spot timestamps stay true across a short loss. Wrappers must
  forward it, as they forward `take_discontinuity()`.
- MAN-310: `[input].bands` (`"all"`, band names or centre frequencies in
  kHz) plans a receiver's streams from the standard CW segments, 160
  through 10 m with 60 m and the NCDXF beacons. `tuning_range_hz` drops
  bands the receiver cannot tune, and a band wider than one stream is
  split, each stream starting tracks only on its own side of the split.
  Several KiwiSDR sessions to one receiver now open side by side. See
  [docs/DECISIONS/2026-10-17-man310-band-plan.md](docs/DECISIONS/2026-10-17-man310-band-plan.md).

- MAN-108: `[detector] reacquire_ms` (default `10000`), how long after
  `hang_ms` a decoded track keeps its identity waiting for its signal to
  return. `0` restores the previous close at `hang_ms`.
- MAN-13: one `manta run` decodes several receivers into one spot output.
  Write `[[input]]` once per receiver (each with a `name`); every input
  decodes on its own thread into one shared validator, so a station two
  receivers hear is spotted once. `calibrate`, `doctor`, `soak` and `check`
  take `--input NAME`; `decode` needs `--freq-correction-ppm` when the
  entries' corrections differ. With several inputs `/healthz` stays `ok`
  while one input is healthy. Single-input runs are unchanged.
- MAN-272: each spot carries its track's latest RST (`5NN` read as `599`)
  and whether it sent `QRL?`, as CW Skimmer's band map shows. JSON spots
  gain `rst` (string or `null`) and `qrlQuery` (boolean), `run --json` and
  `decode --json` spot objects gain `rst`/`qrlQuery`, and the `SPOT:` line
  ends with ` rst=599` and ` QRL?` when present. Telnet and the RBN uplink
  are unchanged. A track merge does not carry the losing track's values.
- `manta-linux-x86_64-soapy.tar.gz`, a release archive whose binary has
  SoapySDR input (`--soapy-driver`, for RTL-SDR, Airspy, SDRplay and
  HackRF) for Ubuntu 24.04 x86_64, so those receivers no longer need a
  build from source. It needs `libsoapysdr0.8` and the receiver's SoapySDR
  module installed; see README's "SoapySDR on Linux x86_64". `manta
  devices` on a binary without Soapy now names this archive (MAN-82).
- JSON/WebSocket spot clients now receive a `hello` first, with schema and session
  identity. Spots and grades carry `seq`; WebSocket `?session=&since=` resumes
  from their shared history bounded by `history_minutes`. New
  `manta_json_stream_resumes_total`, `manta_json_stream_replay_abandoned_total`
  and `manta_json_stream_history_truncated_total` metrics report replay outcomes
  and loss. Consumers must skip hello records (MAN-137).

- MAN-235: `manta doctor --strict` exits 3 when no check failed but the
  verdict is not `DECODING`; without it every verdict still exits 0.
- MAN-234: a `badSamples` field in `decode --json` and `doctor --json` and
  a `bad_samples` field in `calibrate --json` (omitted when 0); a
  `warning: … bad sample(s) …` stderr line from `decode`, `calibrate` and
  `run`/`listen` (at the first bad sample, plus a total at the end if more
  followed); an `input: N bad sample(s) … replaced with silence` line in
  the `doctor` and `soak` reports; and the
  `manta_input_bad_samples_total{source}` counter, at 0 from startup with
  a `[server]` table.
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
- `[detector] non_cw_filter` (default `true`) and `unkeyed_close_ms`
  (default 10000, at least 1000), also as `MANTA_DETECTOR_NON_CW_FILTER`/
  `MANTA_DETECTOR_UNKEYED_CLOSE_MS`; a `NOT_CW` verdict and a
  `detectorConfirmations` count in `manta doctor --json` for a band with
  signals but no keyed CW (MAN-313).
- A Grafana dashboard (spot rate, active tracks, source health, per-target
  uplink status) and Prometheus alert rules (node down, node unhealthy, no
  spots for 30 minutes, uplink target down or flapping) for a node's
  `/metrics`, with promtool tests. See `docs/RUNBOOKS/monitoring.md`
  (MAN-129).
- MAN-140: `--ka9q-group` / `[input] type = "ka9q"` reads one ka9q-radio
  (radiod) IQ channel over RTP multicast; see docs/GUIDES/ka9q-radio.md
- `manta decode`, `manta oracle` and `run`/`check --source-iq` read 24- and
  32-bit integer IQ WAVs (plain PCM, WAVE_FORMAT_EXTENSIBLE, and 24-bit
  samples in 4-byte containers), alongside 32-bit float and 16-bit integer
  (MAN-119).

### Changed

- MAN-139: **breaking for scripts that parse `--json`.**
  `manta decode --json`, `manta run --json`/`listen --json` and
  `manta doctor --json` name fields as the JSON spot stream does:
  camelCase, with the stream's names for shared concepts. `freq_hz` is
  `frequency`. The SNR is `snr` on events and spots (it was `snr_2500_db`
  and `snr_db`) and `snrMin`/`snrMedian`/`snrMax` in doctor, still
  2500 Hz-referenced. A spot's `callsign` is
  `dxCall` and its `confidence` is `decodeConfidence`. `track_id`,
  `sample_ts`, `spot_type` and the other snake_case keys are camelCase. A
  glyph is its character or prosign token (`"A"`, `"<AR>"`) instead of
  `{"Char":"A"}`; `closure` is `signalEnded` or `bookkeeping` beside
  `survivorTrackId`; `spotType` is `cq`, `de`, `beacon` or `unknown`.
  doctor's `duration` is `durationS`, a number of seconds; its verdict and
  check statuses use the text report's spelling (`WEAK_NO_DECODE`, `PASS`);
  `checks_status` is `checksStatus`. Event names and the `{"spot": …}`
  wrapper are unchanged. Every field, and the old names, are in
  `docs/CLI-JSON.md`; the scripts under `scripts/` read the new names, and
  `examples/replay_spots.rs` needs a report regenerated with this build.
  The JSON spot stream (`source` is still `"skimmer"`), the live-state
  stream, telnet and the RBN uplink are unchanged.
- MAN-249: `manta oracle --spotter` has no default and must name the
  reference skimmer's callsign; without it `oracle` exits 2 with a usage
  error.
- MAN-319: a live `[[rbn_uplink]]` (`dry_run = false`) also needs
  `[server].operator_grid` set to a 6-character Maidenhead locator (e.g.
  `"FN03ab"`). `config check`, `run`, `doctor` and a SIGHUP reload refuse a
  live uplink without one, naming the target and the key to set; dry-run
  uplinks (the default) are unchanged. A 4-character grid is refused too.
  `[server]` needs a restart, so a SIGHUP reload goes live only if the
  daemon started with the grid. The registration/settings exchange itself
  stays blocked on MAN-90's Aggregator→RBN capture (claims C8–C11).
- IQ WAV input streams from disk: `manta decode`'s memory no longer grows
  with the recording's length (a 1 GB recording decodes in under 20 MiB), and
  `run --loop` re-opens a recording without re-reading it. A WAV whose data
  chunk is shorter than its header declares fails when the read reaches the
  gap, naming the file (MAN-119).

### Changed

- HPSDR network-radio input and its hardware-free tests are included in
  every build. The `hpsdr` Cargo feature remains an accepted no-op for
  compatibility; SoapySDR still needs `--features soapy` (MAN-239).
- MAN-254: `manta soak --help` says the run lasts the full `--duration`
  even when a `--source` recording ends sooner, instead of repeating
  `run`'s promise that a replay finishes early. Soak's behaviour is unchanged.
- MAN-235: a `manta doctor` run shorter than 60 s that reads
  `WEAK_NO_DECODE` adds a `hint:` line suggesting `--duration 60`.
- The in-repo `wiki/` is the project's canonical wiki. GitHub's Wiki tab
  is not used; disabling it remains an owner step. The wiki's stale
  `verified` stamps and `manifest.json` are refreshed and CI-pinned (MAN-152).

### Fixed

- MAN-343: `manta grade` reads each `--spots`/`--rbn` file once, however it
  is named (directly and through its directory, with `./` or `..`, or
  through a symlink). Before, a file named twice was graded twice, and a
  partly duplicated RBN archive could move the modal frequency or median
  WPM and flip a grade. A hard link is still a different file.
- MAN-423: with a protocol-2 openHPSDR radio, decodes from just before a
  lost datagram run are no longer stamped late by the lost samples (up to
  ~11,900 samples, 62 ms, for 50 datagrams at 192 kS/s); only events at or
  after the loss carry it, on both single- and multi-input runs and during
  startup calibration. A protocol-2 receiver that sat unread while another
  input opened, long enough for its socket buffer to overflow, now counts
  the overflow as dropped datagrams and moves its clock by them, instead of
  reading it as a radio restart of zero length that left later spots early
  by the dropped span.
- MAN-285: `manta status --json` now prints every field the daemon served,
  including fields newer than this build, with object keys in alphabetical
  order. The human-readable `manta status` summary and exit codes are unchanged.
- `ReconnectingSource`, `DecimatingSource` and `manta doctor`'s
  `SampleBoundedSource` now forward the inner source's gap reports
  (`take_discontinuity()` and the new `take_missing_samples()`); they
  swallowed them, so no live source's gap reached the decoder (MAN-305).
- MAN-259: after a live source reconnects, `manta run`'s spot timestamps
  are no longer stamped late by the time the receiver spent reopening while
  already streaming. The reported outage now ends where the stream the
  device returns began, not where its open call returned; for ka9q-radio
  with no SSRC set that was the span its startup stash actually replayed
  (up to the 1.5 s settle window at 192 kS/s, and only ~0.32 s at
  1.536 MS/s, where the stash's 4 MiB cap evicts the opening -- that
  evicted span is outage, not data).
- `manta run` stopped while waiting to reconnect to a lost receiver no
  longer force-decodes the open tracks' last element or judges their
  pending beacons as if the signal had ended; they close as an outage
  (MAN-258).
- `manta calibrate --write` no longer fails on every run with
  `creating temp file …: File exists` after a save was killed before it
  finished, as when manta runs as PID 1 in a container (MAN-335). Each save
  now picks a random temp name and tries another if that one is taken. A
  killed save's `.<config>.calibrate-*.tmp` file is left in place and is
  safe to delete.

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
