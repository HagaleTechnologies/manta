# MAN-74: full TOML configuration surface

Before this ticket, `manta` parsed exactly one operator config file, from
exactly one call site (`manta listen --server-config <path>`), into a type
(`manta_server::config::DaemonConfigFile`) that modeled exactly two of the
six tables `docs/SPEC-decode-core.md` §9 documents: `[server]` and
`[[rbn_uplink]]`. `[input]`, `[spot]`, `[detector]`, and `[decode]` all
parsed without error and did nothing — reproduced live during this
ticket's research: `[input].freq_correction_ppm = 999999`, a value the
equivalent `--freq-correction-ppm` CLI flag rejects at exit code 2, loaded
silently, and a `[detector]`/`[decode]`-bearing file produced
byte-identical decode output to no config file at all.

## Decision 1 — the unified loader lives in `manta-cli`, not `manta-server`

`manta-server` has no dependency on `manta-engine`/`manta-decode`/
`manta-input` (confirmed by reading its `Cargo.toml`), so it cannot hold a
`detector: manta_engine::DetectorConfig` field beside `[server]` without
inverting its current position as a leaf-ish output-layer crate.
`manta-cli` is the only crate with edges to all four crates that own a
table's data. `crates/manta-cli/src/config.rs`'s `ConfigFile` composes:
`server`/`rbn_uplink` (unchanged `manta_server::config` types), `input`
(new `InputSource` enum), `spot` (new `SpotTable`), `detector`/`decode`
(new overlay tables onto `manta_engine::DetectorConfig`/
`manta_decode::decoder::DecodeConfig`).

`manta_server::config::DaemonConfigFile` is deleted. Two config parsers in
the tree was exactly the confusion this ticket exists to remove, and this
repo is the sole consumer of `manta-server` (not published to crates.io),
so removing it from that crate's public API is not a breaking change
anyone outside this repo can observe. `toml` moved from `manta-server`'s
`[dependencies]` to `[dev-dependencies]` — it was only ever exercised by
that crate's own tests once `DaemonConfigFile`'s parsing left.

## Decision 2 — `deny_unknown_fields` at the top level is now correct

The old `DaemonConfigFile` deliberately did NOT `deny_unknown_fields` at
its top level (a round-11 fix, since reverted by deleting the type
entirely): it modeled only two of six real tables, so rejecting unknown
top-level keys would have rejected every valid multi-table config this
repo's own docs describe. Now that all six tables are modeled, that
permissiveness stopped protecting anything — it was silently swallowing
`[detector]`/`[decode]`/`[input]`/`[spot]` settings an operator believed
were in effect. `ConfigFile` denies unknown fields at every level: an
unrecognized top-level table, or an unrecognized key inside a modeled
table, is a parse error naming it.

## Decision 3 — overlay tables, not `#[derive(Deserialize)]` on the domain structs

`[detector]`'s SPEC §9 keys are milliseconds; `DetectorConfig` stores hops
at the channelizer's fixed 375 Hz rate. `[decode]` is one flat SPEC §9
table; `DecodeConfig` nests `DemodConfig`/`BeamConfig`. Neither gap is
bridgeable with plain serde derive attributes on the domain structs
themselves. `DetectorTable`/`DecodeTable` are separate `Option<T>`-field
structs with an `apply(&self, base: &mut …)` method that starts from the
Rust `Default` and overrides only what the file names.

This also settles a real hazard: `manta_engine::DetectorConfig`'s
`on_snr_db` default (12.0 dB) is a *deliberate, empirically-justified*
deviation from SPEC §9's literal stated default (6.0 dB — see that
struct's own `impl Default` doc comment: 6.0 dB produced 298 spurious
tracks against a single clean signal in the V1 golden vector). Because
`resolve_detector`/`resolve_decode` always start from `DetectorConfig::
default()`/`DecodeConfig::default()` and only override named keys, an
operator who omits `on_snr_db` gets the production-safe 12.0, never a
silent regression to the stale spec value — this would NOT be true if the
per-field serde defaults were instead wired from SPEC §9's document text.

Eight SPEC §9 keys (`floor_quantile`, `floor_window_ms`, `block_channels`,
`block_allowance_db` in `[detector]`; `cluster_alpha`, `mu_ratio_bounds`,
`char_gap_dits`, `word_gap_dits` in `[decode]`) are compile-time constants
with no struct field at all — `floor_window_ms`/`block_channels` size
fixed per-channel arrays in the channel-hop hot path, and making either
runtime-settable means heap-allocating there, against the Pi-4 CPU budget
the criterion benches enforce. Each parser still accepts these keys (so
the resulting error can explain *why*, rather than "unknown field", which
would contradict the very spec page an operator is reading) and rejects
them at `apply` time with an actionable error naming where the constant
lives. Making these eight genuinely configurable is a decode-core change,
not a config-surface one, and is tracked as a MAN-74 follow-up rather than
folded into this ticket's scope.

## Decision 4 — `[input]` is an internally-tagged enum, `deny_unknown_fields` on the enum

`#[serde(flatten)]` silently disables `deny_unknown_fields` (documented
serde behavior, confirmed while building this) — a flattened variant makes
every key, including the tag itself, "unknown". Putting `#[serde(tag =
"type", deny_unknown_fields)]` on the enum directly, with the two shared
keys (`freq_correction_ppm`, `dial_freq_hz`) repeated per variant and read
back through an or-pattern accessor, gives strong, specific errors instead
("unknown field `bogus`, expected one of `host`, `port`, `freq_hz`, …",
"unknown variant `rtl`, expected `audio`, `file`, `kiwi`, `soapy`, or
`hpsdr`"). `type` is required whenever `[input]` is present —
`type = "audio"` with no `device` is the "let the OS pick the default
input device" spelling, not a degenerate case.

All five variants parse regardless of which Cargo features (`soapy`,
`hpsdr`) the binary was built with; a config naming a source the binary
can't actually open fails at source-open time (`open_source_spec`,
`main.rs`) with a message naming the required `--features` flag, not as an
opaque type error.

`type = "file"`'s `path`, and `[spot]`'s `blocklist_path`/`notch_path`,
resolve relative to the config file's own directory, not the daemon
process's CWD — a systemd unit with no `WorkingDirectory` set must replay
the WAV / read the blocklist sitting next to `manta.toml`.

## Decision 5 — CLI provenance via `Option<T>`, not retained `ArgMatches`

`--freq-correction-ppm` (on `listen`/`run`/`soak`) drops `default_value_t =
0.0` and becomes `Option<f64>`, keeping its existing `value_parser`.
`Some(0.0)` now unambiguously means "the operator typed 0"; `None` means
"fall through to the file, then the built-in default". Before this change,
`Cli::parse()`'s derived struct always produced a concrete `f64`, whether
or not the flag appeared on the command line — "CLI beats file" cannot
tell those two cases apart without either abandoning `default_value_t`
(the approach taken here, matching the existing pattern for flags like
`dial_freq_hz: Option<f64>`) or retaining `ArgMatches` and querying
`value_source` per field. `manta decode` keeps its `f64` +
`default_value_t = 0.0` unchanged — it has no config-file tier to fall
through to, and stays fully hermetic (see Decision 7).

Any CLI source-selection flag (`--device`/`--source`/`--kiwi-*`/
`--soapy-*`/`--hpsdr-*`) present wins over `[input]` as a whole; mixing a
CLI flag from one source type into a file's different source type has no
coherent meaning, and this rule leaves clap's existing `conflicts_with_all`/
`requires` groups on those flags completely untouched.

## Decision 6 — the environment-variable tier is a TOML-document overlay

`MANTA_<TABLE>_<KEY>` values are spliced into the *parsed TOML document*
(a `toml::Table`) before typed deserialization (`apply_env_overlay`), so
an environment value is validated by exactly the same `deny_unknown_fields`
+ per-field checks a file value is — there is no second, divergent
validation path for env input to drift out of sync with. Recognized table
prefixes: `server`, `input`, `spot`, `detector`, `decode`. `[[rbn_uplink]]`
is excluded on purpose: it's a TOML array-of-tables, and there is no
unambiguous `MANTA_RBN_UPLINK_*` spelling for "the second configured
target" — attempting one is a hard error naming the variable, not a silent
no-op that would leave an operator wondering why a second uplink never
connects. `MANTA_CONFIG` is the one exception: `main.rs` reads it directly
as the `--config` path fallback, not as a table-key overlay.

Value parsing tries the raw text as a TOML scalar first (`9300`, `true`,
`1.5`, `["W1AW","K1ABC"]` all come through typed), falling back to a bare
string otherwise — so `MANTA_SERVER_BIND_ADDR=0.0.0.0` and
`MANTA_SERVER_STATION_CALLSIGN=K1ABC` need no shell quoting.

## Decision 7 — `manta decode`/`manta gen` stay hermetic

Neither gained a `--config` flag or any `MANTA_*` awareness. `decode` is
the entry point SPEC §6's "file input -> byte-identical spot logs"
determinism contract runs through
(`json_output_is_valid_and_deterministic_across_three_runs`); an ambient
config file or environment variable that silently retuned the decoder
would make that contract depend on the machine's environment instead of
the input file alone. Tuning is exercised through `listen --source <wav>
--config <file>` instead, which replays the same WAV deterministically
while still reading the config surface.

## Decision 8 — servers start iff the resolved config has a `[server]` table

`[server]` moved from required (the old `DaemonConfigFile`) to optional on
`ConfigFile` — a `--config` pointing at a `[detector]`/`[decode]`-only
tuning file is now valid and does not start the telnet/JSON/metrics
servers. Every config that previously worked with `--server-config` still
has `[server]` (it was required before), so this is not a behavior change
for existing deployments — only newly-possible ones. A non-empty
`[[rbn_uplink]]` with no `[server]` table is a config-load error (the
uplink's login callsign falls back to `[server].station_callsign`, which
would not exist).

`--config` is the new canonical flag name on `listen`/`soak`;
`--server-config` survives as a deprecated `alias` so existing unit files
keep working unmodified. `manta run` ships as a `visible_alias` of
`listen` — enough to make this ticket's own Gherkin (`manta run --config
manta.toml`) literally executable — without pulling in D11/MAN-77's full
scope (promoting `run` to canonical and retiring `listen`).

## What this unblocks

- **MAN-75** (example config + service unit): its own research deferred
  documenting `[input]`/`[spot]`/`[detector]`/`[decode]` specifically
  because nothing read them before this ticket.
- **MAN-13** (multi-source): `[input]` is a single table today; the
  tagged-enum shape here is the one `[[input]]` (array-of-tables) extends
  into without a breaking rename, mirroring how `[rbn_uplink]` itself
  migrated to `[[rbn_uplink]]` (MAN-32 → MAN-42).
- **MAN-73** (source reconnect): backoff tuning has a natural home as new
  keys on `[input]`'s variants and/or `[[rbn_uplink]]`.
- **MAN-77** (`manta run --config` as the canonical daemon entry point):
  only needs to promote the existing alias and retire `listen`'s name; the
  loader itself is already flag-name-agnostic.

## Explicitly out of scope

- The eight compile-time-constant SPEC §9 keys (Decision 3) — tracked as a
  follow-up, not silently ignored.
- ARCHITECTURE §8's band-plan and runtime `cty`/`scp`-path promises (broad
  review R-09) — neither is named by this ticket's own Gherkin scenarios.
